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

pub(crate) struct Cleanup {
    sender: Option<std::sync::mpsc::Sender<tempfile::TempDir>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Cleanup {
    fn new() -> Self {
        let (sender, receiver) = std::sync::mpsc::channel::<tempfile::TempDir>();
        let worker = std::thread::spawn(move || {
            for stage in receiver {
                let _profile = crate::profile::span("cache", "restore_retired_cleanup");
                let path = stage.path().to_owned();
                if let Err(error) = remove_retired(stage) {
                    qk_executor::status!(
                        "qk: retired output cleanup failed for {}: {error}",
                        path.display()
                    );
                }
            }
        });
        Self {
            sender: Some(sender),
            worker: Some(worker),
        }
    }

    fn retire(&self, stage: tempfile::TempDir) {
        // A stopped worker returns ownership; dropping the stage cleans it here.
        let _ = self.sender.as_ref().unwrap().send(stage);
    }
}

struct RestoreStage<'a> {
    cache: &'a Cache,
    directory: Option<tempfile::TempDir>,
    exchanged: bool,
}

impl<'a> RestoreStage<'a> {
    fn new(cache: &'a Cache, directory: tempfile::TempDir) -> Self {
        Self {
            cache,
            directory: Some(directory),
            exchanged: false,
        }
    }

    fn path(&self) -> &Path {
        self.directory.as_ref().unwrap().path()
    }

    fn retire(&mut self) {
        if self.exchanged {
            self.cache
                .cleanup
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get_or_insert_with(Cleanup::new)
                .retire(self.directory.take().unwrap());
            self.exchanged = false;
        }
    }
}

impl Drop for RestoreStage<'_> {
    fn drop(&mut self) {
        self.retire();
    }
}

fn remove_retired(stage: tempfile::TempDir) -> Result<()> {
    let path = stage.path().to_owned();
    match stage.close() {
        Ok(()) => return Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {}
        Err(error) => return Err(error.into()),
    }
    // Restrictive output modes belong to the live tree. Only this private,
    // retired copy may be made writable to finish removing it.
    for entry in walkdir::WalkDir::new(&path).follow_links(false) {
        let entry = entry?;
        if entry.file_type().is_dir() {
            let metadata = entry.metadata()?;
            set_mode(entry.path(), mode(&metadata) | 0o700)?;
        }
    }
    fs::remove_dir_all(path)?;
    Ok(())
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
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

/// Whether a blob stands at `target` after replacing it went as `renamed`
/// says. Where an open blob cannot be replaced, it is kept only if it holds
/// its content: a restore may take a blob verified earlier this boot without
/// hashing it.
fn kept_or_replaced(renamed: std::io::Result<()>, target: &Path, hash: &str) -> Result<()> {
    match renamed {
        Ok(()) => Ok(()),
        Err(_) if target.is_file() && digest_file(target).is_ok_and(|held| held == hash) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Copies `blob` from the cache to where a restore puts it, and says whether
/// the copy holds the blob's contents. The copy is hashed unless the blob has
/// hashed right since the machine started: a clone shares the blob's data, so
/// hashing it again reads the blob again for nothing.
pub(crate) fn restore_blob(
    source: &Path,
    destination: &Path,
    blob: &str,
    verified: &crate::verified::Verified,
) -> Result<bool> {
    let large = fs::metadata(source)?.len() > SMALL_BLOB_LIMIT as u64;
    if large && verified.contains(blob) {
        fs::copy(source, destination)?;
        return Ok(true);
    }
    let matches = copy_blob_contents(source, destination)? == blob;
    // A small blob is hashed from memory as it is copied, at no cost to note.
    if matches && large {
        verified.record(blob);
    }
    Ok(matches)
}

pub(crate) fn copy_blob_contents(source: &Path, destination: &Path) -> Result<String> {
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

/// Files a thread looks at at least, so a short list is not split.
const LOOKED_PER_THREAD: usize = 128;

/// How long before a file is read it must have last changed for its stamp to
/// tell every later write apart: a write within a timestamp's resolution of
/// the last can leave its size, times and inode as they were. Two seconds
/// covers the coarsest timestamps of the filesystems qk runs on.
const SETTLED: Duration = Duration::from_secs(2);

/// Whether a file with `stamp`, as [`crate::warm::stamp`] makes it, last
/// changed long enough before `started` that any write since changed its
/// change time. Only Unix stamps hold a change time, which every write and
/// replacement sets and no tool sets back; elsewhere nothing is settled.
fn settled(stamp: &[i64], started: SystemTime) -> bool {
    let older = |seconds: i64, nanos: i64| {
        seconds >= 0
            && (0..1_000_000_000).contains(&nanos)
            && started
                .duration_since(
                    SystemTime::UNIX_EPOCH + Duration::new(seconds as u64, nanos as u32),
                )
                .is_ok_and(|age| age > SETTLED)
    };
    cfg!(unix)
        && stamp.len() == 6
        && stamp[1] >= 0
        && older(stamp[1] / 1_000_000_000, stamp[1] % 1_000_000_000)
        && older(stamp[3], stamp[4])
}

/// The blobs of the output files a worktree last published for a set of
/// outputs, each with the file's stamp, so a later publish reads only the
/// files that changed: a rebuild rewrites few of a large target's outputs.
///
/// A file is remembered only when it was read whole between two looks that
/// found the same [`settled`] stamp: no write happened while it was read, and
/// any write since has changed its change time, so a file with that stamp
/// still holds what the blob holds. The blob itself is trusted as a restore
/// trusts a blob it hashed earlier in the same boot: a crash can lose a
/// blob's contents and keep its name, so what another boot remembered is
/// dropped, and the next publish hashes every file, replacing any blob that
/// lost its contents.
#[derive(Default, PartialEq, Serialize, Deserialize)]
struct Published {
    /// The boot the blobs were stored or hashed in, `None` where the boot
    /// cannot be told, and nothing is remembered.
    boot: Option<String>,
    /// Each file's stamp and blob, by its path relative to the workspace.
    files: BTreeMap<String, (Vec<i64>, String)>,
}

impl Published {
    fn new(boot: Option<String>) -> Self {
        Self {
            boot,
            files: BTreeMap::new(),
        }
    }

    /// Where a worktree keeps what it published for `outputs`, named by the
    /// patterns: targets declaring the same outputs share it, at the cost of
    /// files read again, never of a wrong blob.
    fn path(root: &Path, outputs: &Outputs) -> PathBuf {
        let name = blake3::hash(outputs.globs().join("\n").as_bytes()).to_hex();
        paths::worktree_state(root)
            .join("published")
            .join(format!("{}.json", &name[..32]))
    }

    /// What was remembered in this boot, `boot`, and nothing else.
    fn load(root: &Path, outputs: &Outputs, boot: Option<&str>) -> Self {
        let Some(boot) = boot else {
            return Self::default();
        };
        fs::read(Self::path(root, outputs))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Self>(&bytes).ok())
            .filter(|published| published.boot.as_deref() == Some(boot))
            .unwrap_or_default()
    }

    /// The blob of the file at `path`, if it was remembered with `stamp`.
    fn blob(&self, path: &str, stamp: &[i64]) -> Option<&str> {
        self.files
            .get(path)
            .filter(|(known, _)| known.as_slice() == stamp)
            .map(|(_, blob)| blob.as_str())
    }

    /// Replaces the record whole, so a reader never sees half of it. Failing
    /// to keep it costs the next publish its reads only.
    fn save(&self, root: &Path, outputs: &Outputs) {
        let path = Self::path(root, outputs);
        let _ = (|| -> Result<()> {
            let directory = path.parent().context("records have a directory")?;
            fs::create_dir_all(directory)?;
            let mut file = tempfile::NamedTempFile::new_in(directory)?;
            file.write_all(&serde_json::to_vec(self)?)?;
            file.persist(&path)?;
            Ok(())
        })();
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
        // Replacing is atomic and cheaper than re-reading an existing blob.
        let length = fs::metadata(&temporary).map_or(0, |metadata| metadata.len());
        kept_or_replaced(fs::rename(&temporary, &target), &target, &hash)?;
        crate::evict::noted(&self.root, length);
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

    /// Stores the task's outputs and log under `key`: [`Self::gather`], then
    /// [`Self::publish_gathered`], as a run does with the recheck between.
    #[cfg(test)]
    pub(crate) fn publish(
        &self,
        root: &Path,
        key: &str,
        outputs: &Outputs,
        log: &Path,
    ) -> Result<()> {
        let artifacts = self.gather(root, outputs)?;
        self.publish_gathered(key, &artifacts, log)
    }

    /// Stores the blobs of the task's outputs, and returns the artifacts an
    /// entry records for them. An output file whose stamp matches what
    /// [`Published`] remembers keeps that blob unread. Nothing cites the
    /// blobs until [`Self::publish_gathered`] does, so gathering for a result
    /// that is then not kept leaves only blobs eviction removes.
    pub(crate) fn gather(
        &self,
        root: &Path,
        outputs: &Outputs,
    ) -> Result<BTreeMap<String, Artifact>> {
        self.gather_at(root, outputs, SystemTime::now())
    }

    /// [`Self::gather`], begun at `started`, which judges whether a file's
    /// stamp is settled.
    fn gather_at(
        &self,
        root: &Path,
        outputs: &Outputs,
        started: SystemTime,
    ) -> Result<BTreeMap<String, Artifact>> {
        let boot = crate::verified::boot();
        let remembered = Published::load(root, outputs, boot.as_deref());
        // Nothing writes the outputs while they are gathered, so each
        // directory is checked not to be a link once, not once per file in it.
        let directories = std::sync::Mutex::new(foldhash::HashSet::default());
        let mut artifacts = BTreeMap::new();
        let mut pending = Vec::new();
        for entry in outputs.entries(root)? {
            let (path, metadata) = entry?;
            paths::safe_parents_once(root, &path, "cache artifact", &directories)?;
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
                pending.push((path, absolute, metadata));
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
        // The remembered blob of each file left as it was, if the store still
        // holds it.
        let reused =
            crate::hash::in_parallel(&pending, LOOKED_PER_THREAD, |(path, _, metadata)| {
                Ok(remembered
                    .blob(path, &crate::warm::stamp(metadata))
                    .filter(|blob| self.root.join("blobs").join(blob).is_file())
                    .map(str::to_owned))
            })?;
        let sources: Vec<_> = pending
            .iter()
            .zip(&reused)
            .filter(|(_, reused)| reused.is_none())
            .map(|((_, source, _), _)| source.clone())
            .collect();
        let mut blobs = self.put_blobs(&sources).into_iter();
        let mut published = Published::new(boot);
        let mut read = Vec::new();
        for ((path, absolute, metadata), reused) in pending.into_iter().zip(reused) {
            let stamp = crate::warm::stamp(&metadata);
            let blob = match reused {
                Some(blob) => {
                    published.files.insert(path.clone(), (stamp, blob.clone()));
                    blob
                }
                None => {
                    let blob = blobs.next().expect("output blob result")?;
                    if settled(&stamp, started) {
                        read.push((path.clone(), absolute, stamp, blob.clone()));
                    }
                    blob
                }
            };
            artifacts.insert(
                path,
                Artifact::File {
                    blob,
                    mode: mode(&metadata),
                },
            );
        }
        // A file read with the same settled stamp before and after was not
        // written while it was read, so its blob holds what that stamp names.
        let unchanged =
            crate::hash::in_parallel(&read, LOOKED_PER_THREAD, |(_, absolute, stamp, _)| {
                Ok(fs::symlink_metadata(absolute)
                    .is_ok_and(|metadata| crate::warm::stamp(&metadata) == *stamp))
            })?;
        for ((path, _, stamp, blob), unchanged) in read.into_iter().zip(unchanged) {
            if unchanged {
                published.files.insert(path, (stamp, blob));
            }
        }
        if published.boot.is_some() && published != remembered {
            published.save(root, outputs);
        }
        Ok(artifacts)
    }

    /// Stores the log and publishes the entry for `key` with `artifacts`, as
    /// [`Self::gather`] returned them.
    pub(crate) fn publish_gathered(
        &self,
        key: &str,
        artifacts: &BTreeMap<String, Artifact>,
        log: &Path,
    ) -> Result<()> {
        let manifest = ResultRecord::new(key.into(), self.put_blob(log)?, artifacts.clone());
        record::publish(
            &self.root,
            &self.root.join("entries").join(format!("{key}.json")),
            &manifest,
        )
    }

    /// The local entry for `key` with the path of its log, which is checked
    /// whole, or `None` when there is no such entry.
    fn entry(&self, task: &str, key: &str) -> Result<Option<(ResultRecord, PathBuf)>> {
        // Read whole: a File is unbuffered, and serde_json reads byte by byte.
        let header_profile = crate::profile::span(task, "restore_header");
        let bytes = match fs::read(self.root.join("entries").join(format!("{key}.json"))) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let manifest = ResultRecord::read(&bytes, key)?;
        drop(header_profile);
        let _log_profile = crate::profile::span(task, "restore_log_validation");
        let log = self.root.join("blobs").join(&manifest.log);
        if digest_file(&log)? != manifest.log {
            bail!("corrupt cached log");
        }
        qk_executor::read_capture(File::open(&log)?, |_, _| Ok(()))?;
        Ok(Some((manifest, log)))
    }

    /// Whether the local entry for `key` is one [`Self::restore`] would take,
    /// as far as it can tell without touching the task's outputs: its record
    /// and log whole, its outputs within `outputs`, none beneath a file or
    /// link of its own, and every file's blob present, though not hashed.
    /// `Ok(false)` when there is no such entry.
    pub(crate) fn usable(&self, task: &str, key: &str, outputs: &Outputs) -> Result<bool> {
        let Some((manifest, _)) = self.entry(task, key)? else {
            return Ok(false);
        };
        for (path, artifact) in &manifest.artifacts {
            if !outputs.matches(path) {
                bail!("cached artifact is outside declared outputs");
            }
            let mut parent = path.as_str();
            while let Some((above, _)) = parent.rsplit_once('/') {
                if !matches!(
                    manifest.artifacts.get(above),
                    None | Some(Artifact::Directory { .. })
                ) {
                    bail!("cached artifact parent is not a directory: {above}");
                }
                parent = above;
            }
            match artifact {
                Artifact::File { blob, .. } => {
                    if !valid_hash(blob) {
                        bail!("invalid blob identifier");
                    }
                    if !self.root.join("blobs").join(blob).is_file() {
                        bail!("cached artifact missing");
                    }
                }
                Artifact::Directory { .. } => {}
                Artifact::Symlink { target, .. } => validate_link(path, target)?,
            }
        }
        Ok(true)
    }

    /// The output files of the entry for `key`, from the local store or else
    /// the remote store's record of it, or `None` when neither holds one.
    pub(crate) fn artifacts(
        &self,
        key: &str,
        task: &str,
    ) -> Result<Option<BTreeMap<String, Artifact>>> {
        match fs::read(self.root.join("entries").join(format!("{key}.json"))) {
            Ok(bytes) => return Ok(Some(ResultRecord::read(&bytes, key)?.artifacts)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let Some(remote) = &self.remote else {
            return Ok(None);
        };
        let Some(bytes) = remote.fetch_record(key, task)? else {
            return Ok(None);
        };
        Ok(Some(ResultRecord::read(&bytes, key)?.artifacts))
    }

    /// Restores an entry's outputs and log, or returns false when there is no
    /// entry.
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
    ) -> Result<bool> {
        let Some((manifest, log)) = self.entry(task, key)? else {
            return Ok(false);
        };
        let started = SystemTime::now();
        // A stamp mismatch can be an identical rewrite by another runner.
        // Verify changed artifacts before replacing outputs we already hold.
        let held_profile = crate::profile::span(task, "restore_held_outputs");
        let checked = check_outputs(root, task, key, outputs, &manifest);
        if let Some(checked) = &checked
            && checked.repair.is_empty()
        {
            let changed = {
                let kept = self.kept.lock().unwrap();
                checked.revalidated
                    || dependencies
                        .iter()
                        .any(|dependency| !kept.contains(dependency))
            };
            // Stamping is checked against the stamps just taken, and records
            // the outputs from them, rather than walking the outputs again.
            if !changed || stamp_outputs(root, task, key, checked.stamps.clone(), started).is_ok() {
                if !changed {
                    self.kept.lock().unwrap().insert(task.to_owned());
                }
                // Its files and their contents are as listed: stamping them
                // changes only their modification times.
                let files = checked.stamps.iter().filter(|(_, stamp)| stamp[0] != 1);
                self.restoring(
                    task,
                    true,
                    Some(files.map(|(path, _)| path.clone()).collect()),
                );
                qk_executor::replay(File::open(log)?, display, qk_executor::Shown::Kept)?;
                crate::evict::touch(&self.root.join("entries").join(format!("{key}.json")));
                return Ok(true);
            }
        }
        drop(held_profile);
        self.restoring(task, false, None);
        let repair = checked.filter(|checked| !checked.repair.is_empty());
        let planning_profile = crate::profile::span(task, "restore_plan");
        // Stage on the destination filesystem; no existing output is touched yet.
        let stage_parent = paths::worktree_state(root).join("restore");
        fs::create_dir_all(&stage_parent)?;
        let stage = tempfile::Builder::new()
            .prefix("restore-")
            .tempdir_in(stage_parent)?;
        let mut stage = RestoreStage::new(self, stage);
        let mut staging = StagingDirectories::new(stage.path());
        let mut files = Vec::new();
        for (path, artifact) in &manifest.artifacts {
            if repair
                .as_ref()
                .is_some_and(|checked| !checked.repair.contains(path))
            {
                continue;
            }
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
        drop(planning_profile);
        let stage_profile = crate::profile::span(task, "restore_copy_verify");
        self.stage_files(&files, started)?;
        drop(stage_profile);
        let selection_profile = crate::profile::span(task, "restore_select_outputs");
        // Only complete directory selections can promote a staged tree.
        // NOREPLACE below still checks that cleanup left the destination absent.
        let complete = if repair.is_some() {
            Vec::new()
        } else {
            complete_directories(root, outputs, &manifest.artifacts)?
        };
        let current = if let Some(checked) = &repair {
            if output_stamps(root, outputs)? != checked.stamps {
                bail!("outputs changed while staging partial repair");
            }
            checked.repair.clone()
        } else {
            outputs.paths(root)?
        };
        // Validate the complete cleanup set before deleting any output.
        for path in &current {
            paths::safe_parents(root, path)?;
        }
        drop(selection_profile);
        let exchange_profile = crate::profile::span(task, "restore_exchange");
        let mut promoted = BTreeSet::new();
        for path in &complete {
            paths::safe_parents(root, path)?;
            if exchange_directory(&stage.path().join(path), &root.join(path))? {
                stage.exchanged = true;
                promoted.insert(path.clone());
            }
        }
        drop(exchange_profile);
        let cleanup_profile = crate::profile::span(task, "restore_cleanup");
        for path in current.iter().rev() {
            if covered_by(path, &promoted) {
                continue;
            }
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
        drop(cleanup_profile);
        let promotion_profile = crate::profile::span(task, "restore_promote");
        for path in complete {
            if promoted.contains(&path) {
                continue;
            }
            paths::safe_parents(root, &path)?;
            let destination = root.join(&path);
            fs::create_dir_all(destination.parent().context("output has no parent")?)?;
            if promote_directory(&stage.path().join(&path), &destination)? {
                promoted.insert(path);
            }
        }
        for (path, artifact) in &manifest.artifacts {
            if repair
                .as_ref()
                .is_some_and(|checked| !checked.repair.contains(path))
            {
                continue;
            }
            if covered_by(path, &promoted) {
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
        drop(promotion_profile);
        let mode_profile = crate::profile::span(task, "restore_modes");
        for (path, artifact) in manifest.artifacts.iter().rev() {
            if repair
                .as_ref()
                .is_some_and(|checked| !checked.repair.contains(path))
            {
                continue;
            }
            if let Artifact::Directory { mode } = artifact {
                set_mode(&root.join(path), *mode)?;
            }
        }
        drop(mode_profile);
        if repair.is_some() {
            // Outputs kept beside repaired ones are stamped as well, and the
            // one walk that finds them records them all.
            let _profile = crate::profile::span(task, "restore_repair_timestamps");
            stamp_outputs(root, task, key, output_stamps(root, outputs)?, started)?;
        }
        stage.retire();
        if repair.is_none() {
            let _profile = crate::profile::span(task, "restore_record_outputs");
            record_outputs(root, task, key, outputs);
        }
        // Its outputs now hold the entry's artifacts and nothing else: those
        // not replaced were checked to be them.
        let files = manifest
            .artifacts
            .iter()
            .filter(|(_, artifact)| !matches!(artifact, Artifact::Directory { .. }));
        self.restoring(
            task,
            false,
            Some(files.map(|(path, _)| path.clone()).collect()),
        );
        let replay_profile = crate::profile::span(task, "restore_replay");
        qk_executor::replay(File::open(log)?, display, shown)?;
        drop(replay_profile);
        crate::evict::touch(&self.root.join("entries").join(format!("{key}.json")));
        Ok(true)
    }
    /// Fills an already validated private stage before output cleanup begins.
    fn stage_files(&self, files: &[(&str, u32, PathBuf)], time: SystemTime) -> Result<()> {
        let stage = |files: &[(&str, u32, PathBuf)]| -> Result<()> {
            for (blob, mode, destination) in files {
                let source = self.root.join("blobs").join(blob);
                if !restore_blob(&source, destination, blob, self.verified())? {
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

/// How the output files `ran` differ from `recorded`, as `<path> (<how>)`,
/// at most `limit`, the last saying how many more there are.
pub(crate) fn differences(
    ran: &BTreeMap<String, Artifact>,
    recorded: &BTreeMap<String, Artifact>,
    limit: usize,
) -> Vec<String> {
    let paths: BTreeSet<&String> = ran.keys().chain(recorded.keys()).collect();
    let mut differences: Vec<String> = paths
        .into_iter()
        .filter_map(|path| match (ran.get(path), recorded.get(path)) {
            (Some(ran), Some(recorded)) if ran == recorded => None,
            (Some(_), Some(_)) => Some(format!("{path} (differs)")),
            (Some(_), None) => Some(format!("{path} (not in the recorded result)")),
            (None, _) => Some(format!("{path} (not written)")),
        })
        .collect();
    if differences.len() > limit {
        let more = differences.len() - (limit - 1);
        differences.truncate(limit - 1);
        differences.push(format!("{more} more"));
    }
    differences
}

fn covered_by(path: &str, roots: &BTreeSet<String>) -> bool {
    roots.iter().any(|parent| {
        path == parent
            || path
                .strip_prefix(parent)
                .is_some_and(|suffix| suffix.starts_with('/'))
    })
}

fn exchange_directory(staged: &Path, destination: &Path) -> Result<bool> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let metadata = match fs::symlink_metadata(destination) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Ok(false);
        }
        use rustix::fs::{CWD, RenameFlags, renameat_with};
        match renameat_with(CWD, staged, CWD, destination, RenameFlags::EXCHANGE) {
            Ok(()) => Ok(true),
            Err(
                rustix::io::Errno::XDEV
                | rustix::io::Errno::NOSYS
                | rustix::io::Errno::INVAL
                | rustix::io::Errno::OPNOTSUPP
                | rustix::io::Errno::NOENT,
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

/// Sets a file's modification time and leaves its access time. On Linux and
/// macOS this is one system call by path, which does not follow a link;
/// elsewhere it takes a handle: Unix one only to read the file, Windows one
/// that may write its attributes.
pub(crate) fn set_modified(path: &Path, time: SystemTime) -> Result<()> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    if let Ok(since) = time.duration_since(SystemTime::UNIX_EPOCH) {
        use rustix::fs::{AtFlags, CWD, Timespec, Timestamps, UTIME_OMIT, utimensat};
        let times = Timestamps {
            last_access: Timespec {
                tv_sec: 0,
                tv_nsec: UTIME_OMIT,
            },
            last_modification: Timespec {
                tv_sec: since.as_secs() as _,
                tv_nsec: since.subsec_nanos().into(),
            },
        };
        return utimensat(CWD, path, &times, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(std::io::Error::from)
            .with_context(|| format!("cannot set the modification time of {}", path.display()));
    }
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

/// Above how many files a task's outputs are stamped on several threads.
/// Setting a time and reading it back are two short system calls a file, so
/// a thread pays for itself only over a few hundred of them.
const STAMPS_PER_THREAD: usize = 256;

/// Stamps every file among `stamps`, which are a task's outputs as they are
/// now, with `time`, then records the outputs for `key`: `stamps` with each
/// file's read again, since setting a time moves its change time too.
/// Directories and links are left alone, and so are their stamps: setting a
/// time through a link would change the file it points to, and setting a
/// file's time changes nothing about the directory holding it. An error is
/// one setting a time; one reading a stamp back only leaves the outputs
/// unrecorded, so that the next restore checks them again.
fn stamp_outputs(
    root: &Path,
    task: &str,
    key: &str,
    mut stamps: BTreeMap<String, Vec<i64>>,
    time: SystemTime,
) -> Result<()> {
    let _profile = crate::profile::span(task, "restore_stamp_outputs");
    let files: Vec<&str> = stamps
        .iter()
        .filter(|(_, stamp)| stamp[0] == 0)
        .map(|(path, _)| path.as_str())
        .collect();
    let stamped = crate::hash::in_parallel(&files, STAMPS_PER_THREAD, |path| {
        let absolute = root.join(path);
        set_modified(&absolute, time)?;
        Ok(fs::symlink_metadata(&absolute)
            .ok()
            .map(|metadata| output_stamp(&metadata)))
    })?;
    let stamped: Option<Vec<(String, Vec<i64>)>> = files
        .iter()
        .zip(stamped)
        .map(|(path, stamp)| Some(((*path).to_owned(), stamp?)))
        .collect();
    match stamped {
        Some(stamped) => {
            stamps.extend(stamped);
            write_outputs_record(root, task, key, stamps);
        }
        None => {
            let _ = fs::remove_file(outputs_record(root, task));
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
        stamps.insert(path, output_stamp(&metadata));
    }
    Ok(stamps)
}

/// One output's stamp, from its metadata read without following a link:
/// its kind first, 0 for a file, 1 for a directory and 2 for a link.
fn output_stamp(metadata: &fs::Metadata) -> Vec<i64> {
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
        i64::from(mode(metadata)),
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
    stamp
}

#[derive(Serialize, Deserialize)]
struct OutputRecord {
    key: String,
    outputs: BTreeMap<String, Vec<i64>>,
}

/// Records the outputs a worktree now holds for a task's key.
pub(crate) fn record_outputs(root: &Path, task: &str, key: &str, outputs: &Outputs) {
    if let Ok(stamps) = output_stamps(root, outputs) {
        write_outputs_record(root, task, key, stamps);
    }
}

/// Records `stamps` as the outputs a worktree holds for a task's key.
fn write_outputs_record(root: &Path, task: &str, key: &str, stamps: BTreeMap<String, Vec<i64>>) {
    let record = serde_json::to_vec(&OutputRecord {
        key: key.to_owned(),
        outputs: stamps,
    });
    let path = outputs_record(root, task);
    if let Ok(record) = record
        && fs::create_dir_all(path.parent().expect("records have a directory")).is_ok()
    {
        let _ = fs::write(path, record);
    }
}

/// Verified held outputs and artifacts that must be replaced under the same key.
struct CheckedOutputs {
    revalidated: bool,
    repair: BTreeSet<String>,
    stamps: BTreeMap<String, Vec<i64>>,
}

fn check_outputs(
    root: &Path,
    task: &str,
    key: &str,
    outputs: &Outputs,
    manifest: &ResultRecord,
) -> Option<CheckedOutputs> {
    let text = fs::read(outputs_record(root, task)).ok()?;
    let record: OutputRecord = serde_json::from_slice(&text).ok()?;
    if record.key != key {
        return None;
    }
    let current = output_stamps(root, outputs).ok()?;
    if current == record.outputs {
        return Some(CheckedOutputs {
            revalidated: false,
            repair: BTreeSet::new(),
            stamps: current,
        });
    }
    let verification = crate::profile::span(task, "restore_revalidate_outputs");
    if !current.keys().eq(record.outputs.keys()) || !current.keys().eq(manifest.artifacts.keys()) {
        return None;
    }
    let mut checked_parents = BTreeSet::new();
    let mut repair = BTreeSet::new();
    let repair_limit = manifest.artifacts.len().div_ceil(4).clamp(1, 64);
    for (path, artifact) in &manifest.artifacts {
        if !outputs.matches(path) {
            return None;
        }
        if checked_parents.insert(Path::new(path).parent()?) {
            paths::safe_parents(root, path).ok()?;
        }
        let stamp = &current[path];
        let kind = match artifact {
            Artifact::File { .. } => 0,
            Artifact::Directory { .. } => 1,
            Artifact::Symlink { .. } => 2,
        };
        if stamp[0] != kind {
            return None;
        }
        if record.outputs.get(path) == Some(stamp) {
            continue;
        }
        let absolute = root.join(path);
        let matches = match artifact {
            Artifact::File { blob, mode } => {
                stamp[0] == 0
                    && stamp[3] == i64::from(*mode)
                    && fs::symlink_metadata(&absolute).ok()?.is_file()
                    && digest_file(&absolute).ok()? == *blob
            }
            Artifact::Directory { mode } => stamp[0] == 1 && stamp[3] == i64::from(*mode),
            Artifact::Symlink { target, directory } => {
                validate_link(path, target).ok()?;
                stamp[0] == 2
                    && fs::read_link(&absolute).ok()? == Path::new(target)
                    && (!cfg!(windows) || absolute.is_dir() == *directory)
            }
        };
        if !matches {
            repair.insert(path.clone());
            // Dense changes retain full restoration and directory exchange.
            if repair.len() > repair_limit {
                return None;
            }
        }
    }
    // Partial replacement needs writable parents; restrictive trees retain
    // whole-directory restoration rather than changing live permissions.
    if !repair.is_empty()
        && manifest.artifacts.iter().any(|(path, artifact)| {
            matches!(artifact, Artifact::Directory { .. })
                && current[path][3] & 0o200 == 0
                && cfg!(unix)
        })
    {
        return None;
    }
    // Do not bless an artifact changed while its content was being checked.
    if output_stamps(root, outputs).ok()? != current {
        return None;
    }
    drop(verification);
    Some(CheckedOutputs {
        revalidated: true,
        repair,
        stamps: current,
    })
}

#[cfg(test)]
fn outputs_held(
    root: &Path,
    task: &str,
    key: &str,
    outputs: &Outputs,
    manifest: &ResultRecord,
) -> Option<bool> {
    check_outputs(root, task, key, outputs, manifest)
        .filter(|checked| checked.repair.is_empty())
        .map(|checked| checked.revalidated)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The blob an entry stores for `path`.
    fn published_blob(cache: &Cache, key: &str, path: &str) -> String {
        let manifest = ResultRecord::read(
            &fs::read(cache.root.join("entries").join(format!("{key}.json"))).unwrap(),
            key,
        )
        .unwrap();
        match &manifest.artifacts[path] {
            Artifact::File { blob, .. } => blob.clone(),
            other => panic!("{path} is no file: {:?}", serde_json::to_string(other)),
        }
    }

    /// A workspace with two output files and a log, and a cache in it.
    fn publishing() -> (tempfile::TempDir, Cache, Outputs) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join("out/nested")).unwrap();
        fs::write(root.join("out/nested/kept"), "kept").unwrap();
        fs::write(root.join("out/changed"), "before").unwrap();
        fs::write(root.join("log"), "").unwrap();
        let cache = Cache::new(root.join("cache"));
        cache.initialize().unwrap();
        (temp, cache, Outputs::from_patterns(&["out"]))
    }

    /// Publishes as an hour from now, when every file just written is
    /// settled.
    fn publish_later(cache: &Cache, root: &Path, key: &str, outputs: &Outputs, log: &Path) {
        let later = SystemTime::now() + Duration::from_secs(3600);
        let artifacts = cache.gather_at(root, outputs, later).unwrap();
        cache.publish_gathered(key, &artifacts, log).unwrap();
    }

    #[test]
    fn a_stamp_is_settled_once_both_its_times_are_old_enough() {
        let started = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let stamp = |modified: u64, changed: u64| {
            vec![
                4,
                (modified * 1_000_000_000) as i64,
                0o644,
                changed as i64,
                0,
                7,
            ]
        };
        assert_eq!(settled(&stamp(999_990, 999_990), started), cfg!(unix));
        // Changed within the resolution of the coarsest timestamps.
        assert!(!settled(&stamp(999_990, 999_999), started));
        assert!(!settled(&stamp(999_999, 999_990), started));
        // Times after the publish began, as a clock set back leaves them.
        assert!(!settled(&stamp(1_000_005, 999_990), started));
        assert!(!settled(&stamp(999_990, 1_000_005), started));
        // A stamp without a change time.
        assert!(!settled(&[4, 999_990_000_000_000, 0o644], started));
    }

    #[test]
    fn a_settled_output_left_as_it_was_keeps_its_blob_unread() {
        let Some(boot) = crate::verified::boot() else {
            return;
        };
        if !cfg!(unix) {
            return;
        }
        let (temp, cache, outputs) = publishing();
        let root = temp.path();
        let log = root.join("log");
        let first = "a".repeat(64);
        publish_later(&cache, root, &first, &outputs, &log);
        let kept = published_blob(&cache, &first, "out/nested/kept");
        // Remembered as holding other stored content, the file is not read:
        // its entry takes the blob remembered.
        let other = cache.put_blob(&root.join("out/changed")).unwrap();
        let mut published: Published =
            serde_json::from_slice(&fs::read(Published::path(root, &outputs)).unwrap()).unwrap();
        assert_eq!(published.boot.as_deref(), Some(boot.as_str()));
        assert_eq!(published.files["out/nested/kept"].1, kept);
        published.files.get_mut("out/nested/kept").unwrap().1 = other.clone();
        published.save(root, &outputs);
        // The other file changes, size and all, and is read again.
        fs::write(root.join("out/changed"), "after, longer").unwrap();
        let second = "b".repeat(64);
        publish_later(&cache, root, &second, &outputs, &log);
        assert_eq!(published_blob(&cache, &second, "out/nested/kept"), other);
        assert_eq!(
            published_blob(&cache, &second, "out/changed"),
            digest_file(&root.join("out/changed")).unwrap()
        );
    }

    #[test]
    fn a_file_written_just_before_a_publish_is_read_on_the_next() {
        let (temp, cache, outputs) = publishing();
        let root = temp.path();
        let log = root.join("log");
        let first = "a".repeat(64);
        cache.publish(root, &first, &outputs, &log).unwrap();
        // Written within a timestamp's resolution of the read, the files'
        // stamps cannot tell a later write, so neither is remembered.
        let published: Published = fs::read(Published::path(root, &outputs))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        assert!(published.files.is_empty());
        // Rewritten at once with as many bytes, as an incremental build may.
        fs::write(root.join("out/changed"), "behind").unwrap();
        let second = "b".repeat(64);
        cache.publish(root, &second, &outputs, &log).unwrap();
        assert_eq!(
            published_blob(&cache, &second, "out/changed"),
            digest_file(&root.join("out/changed")).unwrap()
        );
    }

    #[test]
    fn a_remembered_blob_the_store_lost_or_another_boot_stored_is_not_taken() {
        if crate::verified::boot().is_none() || !cfg!(unix) {
            return;
        }
        let (temp, cache, outputs) = publishing();
        let root = temp.path();
        let log = root.join("log");
        let first = "a".repeat(64);
        publish_later(&cache, root, &first, &outputs, &log);
        let kept = published_blob(&cache, &first, "out/nested/kept");
        // Gone from the store, as eviction leaves it: stored again.
        fs::remove_file(cache.root.join("blobs").join(&kept)).unwrap();
        let second = "b".repeat(64);
        publish_later(&cache, root, &second, &outputs, &log);
        assert_eq!(published_blob(&cache, &second, "out/nested/kept"), kept);
        assert_eq!(
            fs::read(cache.root.join("blobs").join(&kept)).unwrap(),
            b"kept"
        );
        // Remembered in another boot, whose crash may have lost the blob's
        // contents: read again.
        let other = cache.put_blob(&root.join("out/changed")).unwrap();
        let mut published: Published =
            serde_json::from_slice(&fs::read(Published::path(root, &outputs)).unwrap()).unwrap();
        published.boot = Some("another boot".into());
        published.files.get_mut("out/nested/kept").unwrap().1 = other;
        published.save(root, &outputs);
        let third = "c".repeat(64);
        publish_later(&cache, root, &third, &outputs, &log);
        assert_eq!(published_blob(&cache, &third, "out/nested/kept"), kept);
    }

    /// An entry is usable where restoring it would not fail before it
    /// touched an output: whole, within the task's outputs, its blobs held.
    #[test]
    fn an_entry_is_usable_only_where_restoring_it_would_go_ahead() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join("out")).unwrap();
        fs::write(root.join("out/file"), "value").unwrap();
        let log = root.join("log");
        fs::write(&log, "").unwrap();
        let cache = Cache::new(root.join("cache"));
        cache.initialize().unwrap();
        let outputs = Outputs::from_patterns(&["out"]);
        let key = "a".repeat(64);
        assert!(!cache.usable("build", &key, &outputs).unwrap());
        cache.publish(root, &key, &outputs, &log).unwrap();
        assert!(cache.usable("build", &key, &outputs).unwrap());
        // Outputs that no longer hold what it saved.
        assert!(
            cache
                .usable("build", &key, &Outputs::from_patterns(&["other"]))
                .is_err()
        );
        let blob = blake3::hash(b"value").to_hex().to_string();
        fs::remove_file(cache.root.join("blobs").join(blob)).unwrap();
        assert!(cache.usable("build", &key, &outputs).is_err());
        let restored = cache.restore(
            root,
            "build",
            &Default::default(),
            &key,
            &outputs,
            &qk_executor::Display::Hidden,
            qk_executor::Shown::LocalCache,
        );
        assert!(restored.is_err());
        // A log that does not hold what it did.
        let other = "b".repeat(64);
        cache.publish(root, &other, &outputs, &log).unwrap();
        let empty = blake3::hash(b"").to_hex().to_string();
        // A well-formed log, so that its digest alone gives it away.
        fs::write(
            cache.root.join("blobs").join(&empty),
            [0u8, 2, 0, 0, 0, b'h', b'i'],
        )
        .unwrap();
        let unusable = cache.usable("build", &other, &outputs).unwrap_err();
        assert!(format!("{unusable:#}").contains("corrupt cached log"));
        let restored = cache.restore(
            root,
            "build",
            &Default::default(),
            &other,
            &outputs,
            &qk_executor::Display::Hidden,
            qk_executor::Shown::LocalCache,
        );
        assert!(format!("{:#}", restored.unwrap_err()).contains("corrupt cached log"));
        // A file or link with an artifact beneath it, which no directory
        // could hold.
        use serde_json::json;
        fs::write(cache.root.join("blobs").join(&empty), "").unwrap();
        for parent in [
            json!({"type": "File", "blob": empty, "mode": 0o644}),
            json!({"type": "Symlink", "target": "elsewhere", "directory": false}),
        ] {
            let entry = cache.root.join("entries").join(format!("{other}.json"));
            let mut record: serde_json::Value =
                serde_json::from_slice(&fs::read(&entry).unwrap()).unwrap();
            record["artifacts"]["out/file"] = parent;
            record["artifacts"]["out/file/child"] = json!({"type": "Directory", "mode": 0o755});
            fs::write(&entry, serde_json::to_vec(&record).unwrap()).unwrap();
            fs::remove_dir_all(root.join("out")).unwrap();
            let restored = cache.restore(
                root,
                "build",
                &Default::default(),
                &other,
                &outputs,
                &qk_executor::Display::Hidden,
                qk_executor::Shown::LocalCache,
            );
            assert!(restored.is_err());
            fs::create_dir_all(root.join("out")).unwrap();
            assert!(cache.usable("build", &other, &outputs).is_err(), "{record}");
        }
    }

    #[test]
    fn a_failed_replacement_keeps_only_a_blob_that_holds_its_content() {
        let cache = tempfile::tempdir().unwrap();
        let hash = blake3::hash(b"contents").to_hex().to_string();
        let target = cache.path().join(&hash);
        let failed = || Err(std::io::Error::other("cannot replace an open file"));
        assert!(kept_or_replaced(Ok(()), &target, &hash).is_ok());
        // Nothing there, a damaged blob, or a directory in its place: the error.
        assert!(kept_or_replaced(failed(), &target, &hash).is_err());
        fs::write(&target, b"damaged").unwrap();
        assert!(kept_or_replaced(failed(), &target, &hash).is_err());
        fs::remove_file(&target).unwrap();
        fs::create_dir(&target).unwrap();
        assert!(kept_or_replaced(failed(), &target, &hash).is_err());
        fs::remove_dir(&target).unwrap();
        // One that holds its content is kept.
        fs::write(&target, b"contents").unwrap();
        assert!(kept_or_replaced(failed(), &target, &hash).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn a_blob_that_cannot_be_replaced_is_kept_only_if_it_holds_its_content() {
        use std::os::unix::fs::PermissionsExt;
        // A read-only directory is what stops the replacement, and root
        // writes in it regardless.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let cache = tempfile::tempdir().unwrap();
        let store = Cache::new(cache.path().to_owned());
        store.initialize().unwrap();
        let source = cache.path().join("output");
        fs::write(&source, b"contents").unwrap();
        let blob = blake3::hash(b"contents").to_hex().to_string();
        let blobs = cache.path().join("blobs");
        let target = blobs.join(&blob);
        let read_only = |blobs: &Path, read_only: bool| {
            fs::set_permissions(
                blobs,
                fs::Permissions::from_mode(if read_only { 0o555 } else { 0o755 }),
            )
            .unwrap()
        };
        // A damaged blob the store cannot replace fails the save.
        fs::write(&target, b"damaged").unwrap();
        read_only(&blobs, true);
        let damaged = store.put_blob(&source);
        read_only(&blobs, false);
        assert!(damaged.is_err());
        // One that holds its content needs no replacing.
        fs::write(&target, b"contents").unwrap();
        read_only(&blobs, true);
        let kept = store.put_blob(&source);
        read_only(&blobs, false);
        assert_eq!(kept.unwrap(), blob);
    }

    #[test]
    fn a_large_blob_is_hashed_on_its_first_restore_of_a_boot_only() {
        let cache = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let contents = vec![7u8; SMALL_BLOB_LIMIT * 4];
        let blob = blake3::hash(&contents).to_hex().to_string();
        let source = cache.path().join(&blob);
        fs::write(&source, &contents).unwrap();
        let verified = crate::verified::Verified::load(cache.path());
        let restore = |name: &str, verified: &crate::verified::Verified| {
            restore_blob(&source, &out.path().join(name), &blob, verified).unwrap()
        };
        // A blob the disk damaged is caught on its first restore.
        fs::write(&source, vec![8u8; SMALL_BLOB_LIMIT * 4]).unwrap();
        assert!(!restore("damaged", &verified));
        fs::write(&source, &contents).unwrap();
        assert!(restore("first", &verified));
        if !verified.available() {
            // Where the boot cannot be told, every restore hashes.
            assert!(!verified.contains(&blob));
            return;
        }
        assert!(verified.contains(&blob));
        // Verified once, it is not hashed again until the next boot: a change
        // to it since, which only the disk could make, goes unnoticed.
        fs::write(&source, vec![9u8; SMALL_BLOB_LIMIT * 4]).unwrap();
        assert!(restore("second", &verified));
        // A new boot's list starts empty, and hashes it again.
        fs::remove_dir_all(cache.path().join("verified")).unwrap();
        assert!(!restore(
            "after boot",
            &crate::verified::Verified::load(cache.path())
        ));
    }

    #[test]
    fn differences_name_each_output_that_differs_and_count_the_rest() {
        let file = |blob: &str| Artifact::File {
            blob: blob.repeat(64),
            mode: 0o644,
        };
        let ran = BTreeMap::from([
            ("a".to_owned(), file("1")),
            ("b".to_owned(), file("2")),
            ("c".to_owned(), file("3")),
        ]);
        let recorded = BTreeMap::from([
            ("a".to_owned(), file("1")),
            ("b".to_owned(), file("4")),
            ("d".to_owned(), file("5")),
        ]);
        assert!(differences(&ran, &ran, 5).is_empty());
        assert_eq!(
            differences(&ran, &recorded, 5),
            [
                "b (differs)",
                "c (not in the recorded result)",
                "d (not written)"
            ]
        );
        assert_eq!(differences(&ran, &recorded, 2), ["b (differs)", "2 more"]);
    }

    #[test]
    fn sparse_repair_keeps_other_files_and_refreshes_all_timestamps() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join("out/empty")).unwrap();
        for index in 0..130 {
            fs::write(
                root.join(format!("out/file-{index:03}")),
                format!("value-{index}"),
            )
            .unwrap();
        }
        let kept = root.join("out/file-000");
        let changed = root.join("out/file-129");
        #[cfg(unix)]
        symlink("file-000", &root.join("out/link"), false).unwrap();
        let log = root.join("log");
        fs::write(&log, "").unwrap();
        let cache = Cache::new(root.join("cache"));
        cache.initialize().unwrap();
        let outputs = Outputs::from_patterns(&["out"]);
        let key = "a".repeat(64);
        cache.publish(root, &key, &outputs, &log).unwrap();
        record_outputs(root, "build", &key, &outputs);
        #[cfg(unix)]
        let inode = {
            use std::os::unix::fs::MetadataExt;
            fs::metadata(&kept).unwrap().ino()
        };
        fs::write(&changed, "edited").unwrap();
        #[cfg(unix)]
        {
            fs::remove_file(root.join("out/link")).unwrap();
            symlink("file-129", &root.join("out/link"), false).unwrap();
            set_mode(&changed, mode(&fs::metadata(&changed).unwrap()) ^ 0o100).unwrap();
        }
        let before = SystemTime::now();
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
                .unwrap()
        );
        assert_eq!(fs::read_to_string(&changed).unwrap(), "value-129");
        assert_eq!(fs::read_to_string(&kept).unwrap(), "value-0");
        #[cfg(unix)]
        {
            assert_eq!(
                fs::read_link(root.join("out/link")).unwrap(),
                Path::new("file-000")
            );
            assert_eq!(
                mode(&fs::metadata(&changed).unwrap()),
                mode(&fs::metadata(&kept).unwrap())
            );
        }
        assert_eq!(
            fs::metadata(&kept).unwrap().modified().unwrap(),
            fs::metadata(&changed).unwrap().modified().unwrap()
        );
        assert!(fs::metadata(&kept).unwrap().modified().unwrap() >= before);
        assert!(cache.cleanup.lock().unwrap().is_none());
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(fs::metadata(&kept).unwrap().ino(), inode);
        }
        for index in 0..130 {
            fs::write(root.join(format!("out/file-{index:03}")), "dense edit").unwrap();
        }
        let manifest = ResultRecord::read(
            &fs::read(cache.root.join("entries").join(format!("{key}.json"))).unwrap(),
            &key,
        )
        .unwrap();
        assert!(check_outputs(root, "build", &key, &outputs, &manifest).is_none());
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
                .unwrap()
        );
        assert_eq!(fs::read_to_string(&kept).unwrap(), "value-0");
        assert_eq!(fs::read_to_string(&changed).unwrap(), "value-129");
    }

    #[test]
    fn corrupt_repair_blob_leaves_changed_and_kept_outputs_untouched() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir(root.join("out")).unwrap();
        fs::write(root.join("out/keep"), "keep").unwrap();
        fs::write(root.join("out/change"), "cached").unwrap();
        let log = root.join("log");
        fs::write(&log, "").unwrap();
        let cache = Cache::new(root.join("cache"));
        cache.initialize().unwrap();
        let outputs = Outputs::from_patterns(&["out"]);
        let key = "9".repeat(64);
        cache.publish(root, &key, &outputs, &log).unwrap();
        record_outputs(root, "build", &key, &outputs);
        fs::write(root.join("out/change"), "edited").unwrap();
        let before = output_stamps(root, &outputs).unwrap();
        fs::write(
            cache
                .root
                .join("blobs")
                .join(blake3::hash(b"cached").to_hex().as_str()),
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
        assert_eq!(output_stamps(root, &outputs).unwrap(), before);
        assert_eq!(
            fs::read_to_string(root.join("out/change")).unwrap(),
            "edited"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn exchanged_outputs_are_ready_while_retired_tree_waits_for_cleanup() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join("out/nested")).unwrap();
        fs::write(root.join("out/nested/value"), "cached").unwrap();
        let log = root.join("log");
        fs::write(&log, "").unwrap();
        let cache = Cache::new(root.join("cache"));
        cache.initialize().unwrap();
        let outputs = Outputs::from_patterns(&["out"]);
        let key = "c".repeat(64);
        cache.publish(root, &key, &outputs, &log).unwrap();
        fs::write(root.join("out/nested/value"), "old").unwrap();
        fs::write(root.join("outside"), "keep").unwrap();
        symlink("../outside", &root.join("out/stale-link"), false).unwrap();
        let (sender, receiver) = std::sync::mpsc::channel::<tempfile::TempDir>();
        let (seen, observed) = std::sync::mpsc::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let stage = receiver.recv().unwrap();
            seen.send(stage.path().to_owned()).unwrap();
            wait.recv().unwrap();
            stage.close().unwrap();
        });
        *cache.cleanup.lock().unwrap() = Some(Cleanup {
            sender: Some(sender),
            worker: Some(worker),
        });
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
                .unwrap()
        );
        let retired = observed.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(
            fs::read_to_string(root.join("out/nested/value")).unwrap(),
            "cached"
        );
        assert!(!root.join("out/stale-link").exists());
        assert_eq!(
            fs::read_to_string(retired.join("out/nested/value")).unwrap(),
            "old"
        );
        release.send(()).unwrap();
        cache.cleanup.lock().unwrap().take();
        assert!(!retired.exists());
        assert_eq!(fs::read_to_string(root.join("outside")).unwrap(), "keep");
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_later_exchange_error_retires_readonly_outputs_without_changing_the_error() {
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join("out/nested")).unwrap();
        fs::create_dir_all(root.join("parent/later")).unwrap();
        fs::write(root.join("out/nested/value"), "cached").unwrap();
        fs::write(root.join("parent/later/value"), "cached-later").unwrap();
        set_mode(&root.join("out/nested"), 0o500).unwrap();
        let log = root.join("log");
        fs::write(&log, "").unwrap();
        let cache = Cache::new(root.join("cache"));
        cache.initialize().unwrap();
        let outputs = Outputs::from_patterns(&["out", "parent/later"]);
        let key = "d".repeat(64);
        cache.publish(root, &key, &outputs, &log).unwrap();
        fs::write(root.join("out/nested/value"), "old").unwrap();
        fs::write(root.join("parent/later/value"), "old-later").unwrap();
        fs::write(root.join("out/stale"), "stale").unwrap();
        set_mode(&root.join("parent"), 0o500).unwrap();
        let result = cache.restore(
            root,
            "build",
            &Default::default(),
            &key,
            &outputs,
            &qk_executor::Display::Hidden,
            qk_executor::Shown::LocalCache,
        );
        set_mode(&root.join("parent"), 0o700).unwrap();
        let error = result.unwrap_err();
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        let live_mode = mode(&fs::metadata(root.join("out/nested")).unwrap());
        cache.cleanup.lock().unwrap().take();
        assert_eq!(
            fs::read_to_string(root.join("out/nested/value")).unwrap(),
            "cached"
        );
        assert!(!root.join("out/stale").exists());
        assert_eq!(
            fs::read_to_string(root.join("parent/later/value")).unwrap(),
            "old-later"
        );
        assert_eq!(
            mode(&fs::metadata(root.join("out/nested")).unwrap()),
            live_mode
        );
        assert_eq!(
            fs::read_dir(paths::worktree_state(root).join("restore"))
                .unwrap()
                .count(),
            0
        );
        set_mode(&root.join("out/nested"), 0o700).unwrap();
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn directory_exchange_does_not_replace_files_links_or_absent_paths() {
        let temp = tempfile::tempdir().unwrap();
        let staged = temp.path().join("staged");
        fs::create_dir(&staged).unwrap();
        let destination = temp.path().join("destination");
        assert!(!exchange_directory(&staged, &destination).unwrap());
        fs::write(&destination, "keep").unwrap();
        assert!(!exchange_directory(&staged, &destination).unwrap());
        fs::remove_file(&destination).unwrap();
        symlink("staged", &destination, true).unwrap();
        assert!(!exchange_directory(&staged, &destination).unwrap());
        assert!(
            fs::symlink_metadata(&destination)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn retired_readonly_directories_are_removed_without_changing_live_modes() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join("out/nested")).unwrap();
        fs::write(root.join("out/nested/value"), "cached").unwrap();
        set_mode(&root.join("out/nested"), 0o500).unwrap();
        let log = root.join("log");
        fs::write(&log, "").unwrap();
        let cache = Cache::new(root.join("cache"));
        cache.initialize().unwrap();
        let outputs = Outputs::from_patterns(&["out"]);
        let key = "b".repeat(64);
        cache.publish(root, &key, &outputs, &log).unwrap();
        cache
            .restore(
                root,
                "build",
                &Default::default(),
                &key,
                &outputs,
                &qk_executor::Display::Hidden,
                qk_executor::Shown::LocalCache,
            )
            .unwrap();
        cache.cleanup.lock().unwrap().take();
        assert_eq!(
            fs::read_dir(paths::worktree_state(root).join("restore"))
                .unwrap()
                .count(),
            0
        );
        assert_eq!(mode(&fs::metadata(root.join("out/nested")).unwrap()), 0o500);
        set_mode(&root.join("out/nested"), 0o700).unwrap();
    }

    #[test]
    fn identical_rewrites_are_verified_and_kept_with_fresh_timestamps() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join("out/empty")).unwrap();
        let file = root.join("out/value");
        fs::write(&file, "original").unwrap();
        let log = root.join("log");
        fs::write(&log, "").unwrap();
        let cache = Cache::new(root.join("cache"));
        cache.initialize().unwrap();
        let outputs = Outputs::from_patterns(&["out"]);
        let key = "d".repeat(64);
        cache.publish(root, &key, &outputs, &log).unwrap();
        record_outputs(root, "build", &key, &outputs);
        fs::write(&file, "original").unwrap();
        set_modified(&file, SystemTime::UNIX_EPOCH + Duration::from_secs(100)).unwrap();
        #[cfg(unix)]
        let inode = {
            use std::os::unix::fs::MetadataExt;
            fs::metadata(&file).unwrap().ino()
        };
        let before = SystemTime::now();
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
                .unwrap()
        );
        assert!(fs::metadata(&file).unwrap().modified().unwrap() >= before);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(fs::metadata(&file).unwrap().ino(), inode);
        }
        let manifest = ResultRecord::read(
            &fs::read(cache.root.join("entries").join(format!("{key}.json"))).unwrap(),
            &key,
        )
        .unwrap();
        assert_eq!(
            outputs_held(root, "build", &key, &outputs, &manifest),
            Some(false)
        );
    }

    /// Held outputs are stamped again after a dependency changed, enough of
    /// them to take several threads, and recorded as they are then, so that
    /// the next restore finds them unchanged without hashing them. A link
    /// keeps its own time.
    #[test]
    fn held_outputs_are_stamped_after_a_changed_dependency_and_recorded() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join("out/nested")).unwrap();
        let count = 3 * STAMPS_PER_THREAD;
        for index in 0..count {
            fs::write(root.join(format!("out/nested/file-{index:04}")), "value").unwrap();
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink("nested/file-0000", root.join("out/link")).unwrap();
        let log = root.join("log");
        fs::write(&log, "").unwrap();
        let cache = Cache::new(root.join("cache"));
        cache.initialize().unwrap();
        let outputs = Outputs::from_patterns(&["out"]);
        let key = "f".repeat(64);
        cache.publish(root, &key, &outputs, &log).unwrap();
        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        for index in 0..count {
            set_modified(&root.join(format!("out/nested/file-{index:04}")), old).unwrap();
        }
        let link_time = || {
            fs::symlink_metadata(root.join("out/link"))
                .ok()
                .map(|metadata| metadata.modified().unwrap())
        };
        let link_before = link_time();
        record_outputs(root, "build", &key, &outputs);
        let manifest = ResultRecord::read(
            &fs::read(cache.root.join("entries").join(format!("{key}.json"))).unwrap(),
            &key,
        )
        .unwrap();
        let restore = |dependencies: &BTreeSet<String>| {
            cache
                .restore(
                    root,
                    "build",
                    dependencies,
                    &key,
                    &outputs,
                    &qk_executor::Display::Hidden,
                    qk_executor::Shown::LocalCache,
                )
                .unwrap()
        };
        let before = SystemTime::now();
        // A dependency that is not among the kept changed in this run.
        assert!(restore(&BTreeSet::from(["dependency".to_owned()])));
        for index in 0..count {
            let file = root.join(format!("out/nested/file-{index:04}"));
            assert!(fs::metadata(&file).unwrap().modified().unwrap() >= before);
        }
        assert_eq!(link_time(), link_before);
        assert!(!cache.kept.lock().unwrap().contains("build"));
        // Recorded as stamped: held, and found so without being hashed.
        assert_eq!(
            outputs_held(root, "build", &key, &outputs, &manifest),
            Some(false)
        );
        assert_eq!(
            output_stamps(root, &outputs).unwrap(),
            check_outputs(root, "build", &key, &outputs, &manifest)
                .unwrap()
                .stamps
        );
        // With nothing changed before it, the outputs are kept as they are.
        let stamped = output_stamps(root, &outputs).unwrap();
        assert!(restore(&BTreeSet::new()));
        assert_eq!(output_stamps(root, &outputs).unwrap(), stamped);
        assert!(cache.kept.lock().unwrap().contains("build"));
    }

    /// Setting a link's time sets the link's own, never its target's.
    #[cfg(unix)]
    #[test]
    fn setting_a_time_does_not_follow_a_link() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target");
        fs::write(&target, "target").unwrap();
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        set_modified(&target, old).unwrap();
        set_modified(&link, SystemTime::UNIX_EPOCH + Duration::from_secs(200)).unwrap();
        assert_eq!(fs::metadata(&target).unwrap().modified().unwrap(), old);
    }

    #[test]
    fn revalidation_rejects_changed_contents_and_output_sets() {
        for change in ["contents", "extra", "missing", "kind", "empty-directory"] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            fs::create_dir_all(root.join("out/empty")).unwrap();
            fs::write(root.join("out/value"), "original").unwrap();
            let log = root.join("log");
            fs::write(&log, "").unwrap();
            let cache = Cache::new(root.join("cache"));
            cache.initialize().unwrap();
            let outputs = Outputs::from_patterns(&["out"]);
            let key = "e".repeat(64);
            cache.publish(root, &key, &outputs, &log).unwrap();
            record_outputs(root, "build", &key, &outputs);
            match change {
                "contents" => {
                    let modified = fs::metadata(root.join("out/value"))
                        .unwrap()
                        .modified()
                        .unwrap();
                    fs::write(root.join("out/value"), "modified").unwrap();
                    #[cfg(unix)]
                    set_modified(&root.join("out/value"), modified).unwrap();
                    #[cfg(not(unix))]
                    let _ = modified;
                }
                "extra" => fs::write(root.join("out/extra"), "extra").unwrap(),
                "missing" => fs::remove_file(root.join("out/value")).unwrap(),
                "kind" => {
                    fs::remove_file(root.join("out/value")).unwrap();
                    fs::create_dir(root.join("out/value")).unwrap();
                }
                "empty-directory" => fs::remove_dir(root.join("out/empty")).unwrap(),
                _ => unreachable!(),
            }
            let manifest = ResultRecord::read(
                &fs::read(cache.root.join("entries").join(format!("{key}.json"))).unwrap(),
                &key,
            )
            .unwrap();
            assert_eq!(
                outputs_held(root, "build", &key, &outputs, &manifest),
                None,
                "{change}"
            );
            cache
                .restore(
                    root,
                    "build",
                    &Default::default(),
                    &key,
                    &outputs,
                    &qk_executor::Display::Hidden,
                    qk_executor::Shown::LocalCache,
                )
                .unwrap();
            assert_eq!(fs::read(root.join("out/value")).unwrap(), b"original");
            assert!(root.join("out/empty").is_dir());
            assert!(!root.join("out/extra").exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn revalidation_checks_modes_and_symlink_targets() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir(root.join("out")).unwrap();
        fs::write(root.join("out/value"), "original").unwrap();
        symlink("value", &root.join("out/link"), false).unwrap();
        let log = root.join("log");
        fs::write(&log, "").unwrap();
        let cache = Cache::new(root.join("cache"));
        cache.initialize().unwrap();
        let outputs = Outputs::from_patterns(&["out"]);
        let key = "f".repeat(64);
        cache.publish(root, &key, &outputs, &log).unwrap();
        record_outputs(root, "build", &key, &outputs);
        let manifest = ResultRecord::read(
            &fs::read(cache.root.join("entries").join(format!("{key}.json"))).unwrap(),
            &key,
        )
        .unwrap();
        let original_mode = mode(&fs::metadata(root.join("out/value")).unwrap());
        set_mode(&root.join("out/value"), original_mode ^ 0o100).unwrap();
        assert_eq!(outputs_held(root, "build", &key, &outputs, &manifest), None);
        set_mode(&root.join("out/value"), original_mode).unwrap();
        fs::remove_file(root.join("out/link")).unwrap();
        symlink("value", &root.join("out/link"), false).unwrap();
        assert_eq!(
            outputs_held(root, "build", &key, &outputs, &manifest),
            Some(true)
        );
        fs::remove_file(root.join("out/link")).unwrap();
        symlink("other", &root.join("out/link"), false).unwrap();
        assert_eq!(outputs_held(root, "build", &key, &outputs, &manifest), None);
    }

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
        cache.publish(root, &key, &outputs, &log).unwrap();
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
                    .unwrap()
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
                .join(blake3::hash(b"last").to_hex().as_str()),
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
    fn parallel_publication_keeps_artifacts_and_log_deterministic() {
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
        cache.publish(root, &key, &outputs, &log).unwrap();
        let entry = cache.root.join("entries").join(format!("{key}.json"));
        let manifest = fs::read(&entry).unwrap();
        cache.publish(root, &key, &outputs, &log).unwrap();
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
                .unwrap()
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
