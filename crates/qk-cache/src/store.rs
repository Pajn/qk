use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Component, Path};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

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

#[derive(Serialize, Deserialize)]
#[serde(tag = "type")]
enum Artifact {
    File { blob: String, mode: u32 },
    Directory { mode: u32 },
    Symlink { target: String, directory: bool },
}

impl Manifest {
    /// The output fingerprint of the stored artifacts, without reading any file.
    fn output_fingerprint(&self) -> Result<String> {
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
        combine_outputs(&self.key, &files)
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

fn set_mode(path: &Path, mode: u32) -> Result<()> {
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

fn validate_link(path: &str, target: &str) -> Result<()> {
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

fn symlink(target: &str, path: &Path, directory: bool) -> Result<()> {
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

fn valid_hash(value: &str) -> bool {
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

    fn put_blob(&self, source: &Path) -> Result<String> {
        // fs::copy clones on copy-on-write filesystems when source and cache share
        // a volume, and falls back to a byte copy otherwise. Cloning needs a fresh
        // destination path, so copy into a private directory rather than a temp file.
        let staging = tempfile::tempdir_in(self.root.join("tmp"))?;
        let temporary = staging.path().join("blob");
        fs::copy(source, &temporary)?;
        // Blobs keep no meaningful mode of their own; restores apply the manifest's.
        set_mode(&temporary, if cfg!(unix) { 0o644 } else { 0 })?;
        File::open(&temporary)?.sync_all()?;
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
        for path in outputs.paths(root)? {
            paths::safe_parents(root, &path)?;
            let absolute = root.join(&path);
            let metadata = fs::symlink_metadata(&absolute)?;
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
        manifest.output_fingerprint()
    }

    /// Restores an entry's outputs and log, returning its output fingerprint.
    pub(crate) fn restore(
        &self,
        root: &Path,
        key: &str,
        outputs: &Outputs,
    ) -> Result<Option<String>> {
        let file = match File::open(self.root.join("entries").join(format!("{key}.json"))) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let manifest: Manifest = serde_json::from_reader(file)?;
        if manifest.version != 1 || manifest.key != key || !valid_hash(&manifest.log) {
            bail!("invalid cache manifest");
        }
        let log = self.root.join("blobs").join(&manifest.log);
        if digest_file(&log)? != manifest.log {
            bail!("corrupt cached log");
        }
        qk_executor::read_capture(File::open(&log)?, |_, _| Ok(()))?;
        // Stage on the destination filesystem; no existing output is touched yet.
        let stage_parent = root.join(".qk");
        if fs::symlink_metadata(&stage_parent)
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            bail!(".qk cannot be a symlink");
        }
        fs::create_dir_all(&stage_parent)?;
        let stage = tempfile::Builder::new()
            .prefix("restore-")
            .tempdir_in(stage_parent)?;
        for (path, artifact) in &manifest.artifacts {
            paths::safe_parents(root, path)?;
            paths::safe_parents(stage.path(), path)?;
            if !outputs.matches(path) {
                bail!("cached artifact is outside declared outputs");
            }
            let destination = stage.path().join(path);
            fs::create_dir_all(destination.parent().context("output has no parent")?)?;
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
                    set_mode(&destination, *mode)?;
                }
                Artifact::Directory { .. } => {
                    fs::create_dir_all(destination)?;
                }
                Artifact::Symlink { target, directory } => {
                    validate_link(path, target)?;
                    symlink(target, &destination, *directory)?;
                }
            }
        }
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
                    permissions.set_readonly(false);
                    fs::set_permissions(&absolute, permissions)?;
                }
                fs::remove_file(absolute)?;
            }
        }
        for (path, artifact) in &manifest.artifacts {
            paths::safe_parents(root, path)?;
            let destination = root.join(path);
            fs::create_dir_all(destination.parent().context("output has no parent")?)?;
            if matches!(artifact, Artifact::Directory { .. }) {
                fs::create_dir_all(&destination)?;
            } else {
                fs::rename(stage.path().join(path), &destination)?;
            }
        }
        for (path, artifact) in manifest.artifacts.iter().rev() {
            if let Artifact::Directory { mode } = artifact {
                set_mode(&root.join(path), *mode)?;
            }
        }
        qk_executor::replay(File::open(log)?)?;
        manifest.output_fingerprint().map(Some)
    }
}
