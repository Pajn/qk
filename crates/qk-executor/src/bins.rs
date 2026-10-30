//! The `PATH` a declared environment builds: a directory linking each
//! command its `bin` entries name, found on the `PATH` qk inherited, then the
//! directories its `dirs` entries add.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use qk_config::env::Environment;

/// Builds `PATH` for a task in `environment`, finding its commands on
/// `inherited`, the `PATH` qk inherited, and notes where each resolved to for
/// the task's key. `dirs` entries are relative to the workspace root, and can
/// hold `{workspaceRoot}` and `{projectRoot}`.
pub fn path(
    environment: &mut Environment,
    inherited: Option<&OsString>,
    workspace_root: &Path,
    project_root: &str,
) -> Result<OsString> {
    let search: Vec<PathBuf> = inherited
        .map(|path| std::env::split_paths(path).collect())
        .unwrap_or_default();
    let mut found = Vec::new();
    let mut binaries = BTreeMap::new();
    for name in environment.bins() {
        let path = search
            .iter()
            .find_map(|directory| executable(&workspace_root.join(directory), name))
            .with_context(|| {
                format!("the command {name} of the task's environment is not on PATH")
            })?;
        let real = path
            .canonicalize()
            .with_context(|| format!("cannot resolve {}", path.display()))?;
        // By content, the same command keys alike wherever it is installed,
        // and one changed in place, without moving, changes the key.
        let keyed = if environment.keyed_by_content(name) {
            format!("content:{}", content_digest(&real)?)
        } else {
            real.to_string_lossy().into_owned()
        };
        binaries.insert(name.clone(), keyed);
        found.push((name.clone(), path));
    }
    let mut entries = Vec::new();
    if !found.is_empty() {
        entries.extend(links(&found)?);
    }
    for directory in environment.dirs() {
        let expanded = directory
            .replace("{workspaceRoot}", &workspace_root.to_string_lossy())
            .replace("{projectRoot}", project_root);
        entries.push(workspace_root.join(expanded));
    }
    // What a from entry put on PATH comes last.
    if let Some(provided) = environment.provided_path() {
        entries.extend(std::env::split_paths(provided));
    }
    environment.resolved(binaries);
    std::env::join_paths(entries).context("cannot build the task's PATH")
}

/// The digest of the file at `path`, kept in the user's cache directory by
/// its size, modification time and, on Unix, inode, so a large toolchain
/// binary is read once per change rather than once per run.
fn content_digest(path: &Path) -> Result<String> {
    let metadata = path
        .metadata()
        .with_context(|| format!("cannot read {}", path.display()))?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |modified| modified.as_nanos());
    #[cfg(unix)]
    let inode = std::os::unix::fs::MetadataExt::ino(&metadata);
    #[cfg(not(unix))]
    let inode = 0u64;
    let stamp = format!("{} {modified} {inode}", metadata.len());
    let kept = cache_directory().ok().map(|directory| {
        let name = blake3::hash(path.as_os_str().as_encoded_bytes()).to_hex();
        directory.join("digests").join(&name[..32])
    });
    if let Some(digest) = kept
        .as_ref()
        .and_then(|file| std::fs::read_to_string(file).ok())
        .and_then(|text| {
            let (kept_stamp, digest) = text.trim_end().rsplit_once(' ')?;
            (kept_stamp == stamp).then(|| digest.to_owned())
        })
    {
        return Ok(digest);
    }
    let mut hasher = blake3::Hasher::new();
    let mut file =
        std::fs::File::open(path).with_context(|| format!("cannot read {}", path.display()))?;
    std::io::copy(&mut file, &mut hasher)
        .with_context(|| format!("cannot read {}", path.display()))?;
    let digest = hasher.finalize().to_hex().to_string();
    // Kept only if nothing changed it while it was read; failing to keep it
    // only means reading it again.
    if let Some(file) = kept
        && path.metadata().is_ok_and(|now| {
            now.len() == metadata.len() && now.modified().ok() == metadata.modified().ok()
        })
        && let Some(directory) = file.parent()
        && std::fs::create_dir_all(directory).is_ok()
    {
        let partial = file.with_extension(format!("{}.partial", std::process::id()));
        if std::fs::write(&partial, format!("{stamp} {digest}\n")).is_ok()
            && std::fs::rename(&partial, &file).is_err()
        {
            let _ = std::fs::remove_file(&partial);
        }
    }
    Ok(digest)
}

/// The file named `name` in `directory`, if it is one a shell would run.
fn executable(directory: &Path, name: &str) -> Option<PathBuf> {
    let candidates: Vec<PathBuf> = if cfg!(windows) {
        let extensions = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
        std::iter::once(directory.join(name))
            .chain(
                extensions
                    .split(';')
                    .filter(|extension| !extension.is_empty())
                    .map(|extension| directory.join(format!("{name}{extension}"))),
            )
            .collect()
    } else {
        vec![directory.join(name)]
    };
    candidates.into_iter().find(|candidate| {
        let Ok(metadata) = candidate.metadata() else {
            return false;
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
        }
        #[cfg(not(unix))]
        {
            metadata.is_file()
        }
    })
}

/// The `PATH` entries holding `found`, each a command's name and the file
/// found for it: one directory linking each name to its file, named by what
/// it holds so tasks with the same commands share it.
#[cfg(unix)]
fn links(found: &[(String, PathBuf)]) -> Result<Vec<PathBuf>> {
    let mut hasher = blake3::Hasher::new();
    for (name, path) in found {
        hasher.update(name.as_bytes());
        hasher.update(&[0]);
        hasher.update(path.as_os_str().as_encoded_bytes());
        hasher.update(&[0]);
    }
    let digest = hasher.finalize().to_hex();
    let parent = cache_directory()?.join("bin");
    let directory = parent.join(&digest[..32]);
    if !directory.is_dir() {
        std::fs::create_dir_all(&parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
        // Built beside it and renamed into place, so a task sharing it never
        // sees it half made.
        let building = parent.join(format!(
            ".{}.{}.{}",
            &digest[..32],
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos())
        ));
        std::fs::create_dir(&building)
            .with_context(|| format!("cannot create {}", building.display()))?;
        for (name, path) in found {
            std::os::unix::fs::symlink(path, building.join(name))
                .with_context(|| format!("cannot link {name} to {}", path.display()))?;
        }
        if let Err(error) = std::fs::rename(&building, &directory) {
            let _ = std::fs::remove_dir_all(&building);
            // Another task made it first.
            if !directory.is_dir() {
                return Err(error)
                    .with_context(|| format!("cannot create {}", directory.display()));
            }
        }
    }
    Ok(vec![directory])
}

/// On Windows, where making a link takes a privilege a user may not hold,
/// the directory of each command instead, which also offers the commands
/// beside it.
#[cfg(not(unix))]
fn links(found: &[(String, PathBuf)]) -> Result<Vec<PathBuf>> {
    let mut directories: Vec<PathBuf> = Vec::new();
    for (_, path) in found {
        let directory = path
            .parent()
            .context("a command has no directory")?
            .to_owned();
        if !directories.contains(&directory) {
            directories.push(directory);
        }
    }
    Ok(directories)
}

/// The user's own cache directory for qk, which other users cannot write to,
/// unlike the system's temporary directory.
#[cfg(unix)]
pub(crate) fn cache_directory() -> Result<PathBuf> {
    let home = || std::env::var_os("HOME").filter(|home| !home.is_empty());
    let base = if cfg!(target_os = "macos") {
        home().map(|home| PathBuf::from(home).join("Library/Caches"))
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .filter(|cache| Path::new(cache).is_absolute())
            .map(PathBuf::from)
            .or_else(|| home().map(|home| PathBuf::from(home).join(".cache")))
    };
    match base {
        Some(base) => Ok(base.join("qk")),
        None => anyhow::bail!(
            "HOME is not set, so qk has nowhere to link the commands of a task's environment"
        ),
    }
}

/// The user's own cache directory for qk.
#[cfg(not(unix))]
pub(crate) fn cache_directory() -> Result<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .filter(|directory| !directory.is_empty())
        .map(|directory| PathBuf::from(directory).join("qk"))
        .context("LOCALAPPDATA is not set, so qk has nowhere to keep a task environment's output")
}
