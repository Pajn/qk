//! Keeps the cache under its size limit by evicting the least recently used
//! entries, as Nx's `maxCacheSize` does.
//!
//! An entry's last use is its manifest's modification time, refreshed on every
//! hit. Blobs are shared between entries, so a blob is deleted only once no
//! remaining manifest cites it. A blob no manifest cites may belong to an entry
//! another run is still publishing, so it is kept until it is an hour old.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How long an unreferenced blob or scratch file may be part of a publish.
const IN_FLIGHT: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Pruned {
    /// Bytes the cache holds afterwards.
    pub size: u64,
    pub entries: usize,
    pub blobs: usize,
    pub freed: u64,
}

/// Nx's `parseMaxCacheSize`: a number of bytes with an optional `KB`, `MB` or
/// `GB` unit, in powers of 1024.
pub fn parse_size(text: &str) -> Result<u64> {
    let text = text.trim();
    let (number, unit) = match text.find(|c: char| !(c.is_ascii_digit() || c == '.')) {
        Some(at) => (&text[..at], text[at..].trim_start()),
        None => (text, ""),
    };
    let factor: u64 = match unit {
        "" | "B" => 1,
        "KB" => 1 << 10,
        "MB" => 1 << 20,
        "GB" => 1 << 30,
        _ => bail!("invalid cache size {text:?}: use a number with an optional KB, MB or GB"),
    };
    if number.matches('.').count() > 1 || number.is_empty() {
        bail!("invalid cache size {text:?}");
    }
    let number: f64 = number
        .parse()
        .with_context(|| format!("invalid cache size {text:?}"))?;
    Ok((number * factor as f64) as u64)
}

/// The size limit: `NX_MAX_CACHE_SIZE`, then nx.json `maxCacheSize`, then, as
/// in Nx, a tenth of the disk holding the cache. `None` when the limit is 0,
/// which means unlimited.
pub fn max_size(configured: Option<&Value>, cache: &Path) -> Result<Option<u64>> {
    let configured = match std::env::var("NX_MAX_CACHE_SIZE") {
        Ok(value) => Some(value),
        Err(_) => configured.map(|value| match value {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        }),
    };
    let limit = match configured {
        Some(text) => parse_size(&text)?,
        None => {
            let existing = cache
                .ancestors()
                .find(|path| path.exists())
                .context("cache path has no existing ancestor")?;
            fs2::total_space(existing)? / 10
        }
    };
    Ok((limit > 0).then_some(limit))
}

/// Refreshes an entry's last use.
pub(crate) fn touch(manifest: &Path) {
    if let Ok(file) = File::options().write(true).open(manifest) {
        let _ = file.set_modified(SystemTime::now());
    }
}

struct Entry {
    path: std::path::PathBuf,
    used: SystemTime,
    size: u64,
    blobs: BTreeSet<String>,
}

/// Evicts least recently used entries until the cache holds at most `limit`
/// bytes, and removes what no entry needs. Another prune in progress makes
/// this one return without doing anything.
///
/// Explicit pruning always remeasures the cache, including files changed in
/// place by external tools. Normal runs use [`prune_automatic`].
pub fn prune(root: &Path, limit: u64) -> Result<Pruned> {
    prune_inner(root, limit, false)
}

/// Enforces the limit after a run, reusing size accounting while the cache's
/// directories are unchanged. Normal publication replaces records atomically
/// and blobs are immutable, so directory changes invalidate the memo. A full
/// reconciliation still runs at least once an hour.
pub fn prune_automatic(root: &Path, limit: u64) -> Result<Pruned> {
    prune_inner(root, limit, true)
}

fn prune_inner(root: &Path, limit: u64, automatic: bool) -> Result<Pruned> {
    if !root.join("entries").is_dir() {
        return Ok(Pruned::default());
    }
    fs::create_dir_all(root.join("locks"))?;
    let marker = root.join("locks").join(".prune");
    let recent = fs::metadata(&marker)
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age < IN_FLIGHT);
    if automatic && recent {
        let size = cached_size(root)?;
        if size <= limit {
            return Ok(Pruned {
                size,
                ..Pruned::default()
            });
        }
    }
    let guard = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&marker)?;
    if guard.try_lock_exclusive().is_err() {
        return Ok(Pruned::default());
    }
    // Never associate a computed post-eviction size with stamps taken only
    // after eviction: a concurrent publisher could have added uncounted bytes.
    let _ = fs::remove_file(root.join("locks").join(".size"));
    let _ = guard.set_modified(SystemTime::now());
    let now = SystemTime::now();
    let age = |metadata: &fs::Metadata| {
        metadata
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .unwrap_or_default()
    };
    let mut entries = Vec::new();
    let mut references: BTreeMap<String, usize> = BTreeMap::new();
    // Warm state records cite blobs as entries do, and age out the same way.
    let warm = fs::read_dir(root.join("warm")).into_iter().flatten();
    for item in fs::read_dir(root.join("entries"))?.chain(warm) {
        let item = item?;
        let metadata = item.metadata()?;
        // Unreadable manifests are misses already; they only take space.
        let blobs = fs::read(item.path())
            .ok()
            .and_then(|bytes| crate::record::StoredRecord::read(&bytes).ok())
            .map(|record| record.blobs())
            .unwrap_or_default();
        for blob in &blobs {
            *references.entry(blob.clone()).or_default() += 1;
        }
        entries.push(Entry {
            path: item.path(),
            used: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            size: metadata.len(),
            blobs,
        });
    }
    let mut blob_sizes = BTreeMap::new();
    let mut pruned = Pruned::default();
    for item in fs::read_dir(root.join("blobs"))? {
        let item = item?;
        let metadata = item.metadata()?;
        let name = item.file_name().to_string_lossy().into_owned();
        if !references.contains_key(&name) && age(&metadata) > IN_FLIGHT {
            if fs::remove_file(item.path()).is_ok() {
                pruned.blobs += 1;
                pruned.freed += metadata.len();
            }
            continue;
        }
        blob_sizes.insert(name, metadata.len());
    }
    let mut size: u64 =
        entries.iter().map(|entry| entry.size).sum::<u64>() + blob_sizes.values().sum::<u64>();
    entries.sort_by_key(|entry| entry.used);
    for entry in entries {
        if size <= limit {
            break;
        }
        if fs::remove_file(&entry.path).is_err() {
            continue;
        }
        pruned.entries += 1;
        pruned.freed += entry.size;
        size -= entry.size;
        for blob in entry.blobs {
            let count = references.get_mut(&blob).expect("counted above");
            *count -= 1;
            if *count == 0
                && let Some(blob_size) = blob_sizes.remove(&blob)
                && fs::remove_file(root.join("blobs").join(&blob)).is_ok()
            {
                pruned.blobs += 1;
                pruned.freed += blob_size;
                size -= blob_size;
            }
        }
    }
    // Scratch space of runs that ended without cleaning up.
    for item in fs::read_dir(root.join("tmp"))
        .into_iter()
        .flatten()
        .flatten()
    {
        if item
            .metadata()
            .is_ok_and(|metadata| age(&metadata) > IN_FLIGHT)
        {
            let _ = fs::remove_dir_all(item.path()).or_else(|_| fs::remove_file(item.path()));
        }
    }
    // Locks of keys without an entry, unless another run holds them.
    for item in fs::read_dir(root.join("locks"))?.flatten() {
        let name = item.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || root.join("entries").join(format!("{name}.json")).exists() {
            continue;
        }
        if item
            .metadata()
            .is_ok_and(|metadata| age(&metadata) > IN_FLIGHT)
            && let Ok(file) = File::open(item.path())
            && file.try_lock_exclusive().is_ok()
        {
            let _ = fs::remove_file(item.path());
        }
    }
    pruned.size = if automatic { cached_size(root)? } else { size };
    Ok(pruned)
}

// Recent timestamps may have coarse resolution on the backing filesystem.
const STAMP_SETTLE: Duration = Duration::from_secs(2);

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct DirectoryStamp {
    modified: SystemTime,
    len: u64,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64),
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct SizeMemo {
    measured: SystemTime,
    stamps: Vec<Option<DirectoryStamp>>,
    size: u64,
}

fn directory_stamps(root: &Path) -> Result<Vec<Option<DirectoryStamp>>> {
    ["entries", "blobs", "warm"]
        .iter()
        .map(|name| {
            let metadata = match fs::metadata(root.join(name)) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error.into()),
            };
            #[cfg(unix)]
            use std::os::unix::fs::MetadataExt;
            Ok(Some(DirectoryStamp {
                modified: metadata.modified()?,
                len: metadata.len(),
                #[cfg(unix)]
                identity: (
                    metadata.dev(),
                    metadata.ino(),
                    metadata.ctime(),
                    metadata.ctime_nsec(),
                ),
            }))
        })
        .collect()
}

fn settled(stamps: &[Option<DirectoryStamp>], now: SystemTime) -> bool {
    stamps.iter().flatten().all(|stamp| {
        let old_enough = |time| {
            now.duration_since(time)
                .is_ok_and(|age| age >= STAMP_SETTLE)
        };
        if !old_enough(stamp.modified) {
            return false;
        }
        #[cfg(unix)]
        {
            let (_, _, seconds, nanos) = stamp.identity;
            if seconds < 0 || !(0..1_000_000_000).contains(&nanos) {
                return false;
            }
            let changed = SystemTime::UNIX_EPOCH + Duration::new(seconds as u64, nanos as u32);
            if !old_enough(changed) {
                return false;
            }
        }
        true
    })
}

fn cached_size(root: &Path) -> Result<u64> {
    let memo_path = root.join("locks").join(".size");
    let now = SystemTime::now();
    let before = directory_stamps(root)?;
    if settled(&before, now)
        && let Some(memo) = fs::read(&memo_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<SizeMemo>(&bytes).ok())
        && now
            .duration_since(memo.measured)
            .is_ok_and(|age| age < IN_FLIGHT)
        && memo.stamps == before
    {
        return Ok(memo.size);
    }
    let size = ["entries", "blobs", "warm"]
        .iter()
        .zip(&before)
        .filter(|(_, stamp)| stamp.is_some())
        .try_fold(0u64, |total, (name, _)| {
            directory_size(&root.join(name)).map(|size| total.saturating_add(size))
        })?;
    let after = directory_stamps(root)?;
    if before == after && settled(&after, now) {
        let memo = SizeMemo {
            measured: now,
            stamps: after,
            size,
        };
        // Concurrent writers can replace this memo safely: readers still compare
        // its stamps with the current directories. Partial writes are misses.
        let mut temporary = tempfile::NamedTempFile::new_in(root.join("locks"))?;
        serde_json::to_writer(temporary.as_file_mut(), &memo)?;
        temporary.persist(&memo_path).map_err(|error| error.error)?;
    } else {
        let _ = fs::remove_file(memo_path);
    }
    Ok(size)
}

fn directory_size(directory: &Path) -> Result<u64> {
    let mut size = 0;
    for item in fs::read_dir(directory)? {
        size += item?.metadata()?.len();
    }
    Ok(size)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        for name in ["entries", "blobs", "locks"] {
            fs::create_dir(root.path().join(name)).unwrap();
        }
        fs::write(root.path().join("entries/a.json"), b"bad record").unwrap();
        fs::write(root.path().join("blobs/a"), [0; 100]).unwrap();
        fs::write(root.path().join("locks/.prune"), []).unwrap();
        root
    }

    #[test]
    fn automatic_size_memo_reuses_and_reconciles() {
        let root = fixture();
        let root = root.path();
        // No memo can be trusted while directory timestamps are recent.
        assert_eq!(prune_automatic(root, 1_000).unwrap().size, 110);
        assert!(!root.join("locks/.size").exists());
        std::thread::sleep(STAMP_SETTLE + Duration::from_millis(100));
        assert_eq!(prune_automatic(root, 1_000).unwrap().size, 110);
        assert!(root.join("locks/.size").exists());

        // Unsupported external in-place changes intentionally demonstrate that
        // the automatic path reads only directory stamps, rather than each file.
        fs::write(root.join("blobs/a"), [0; 200]).unwrap();
        touch(&root.join("entries/a.json"));
        assert_eq!(prune_automatic(root, 1_000).unwrap().size, 110);
        assert_eq!(prune(root, 1_000).unwrap().size, 210);
        assert!(!root.join("locks/.size").exists());

        assert_eq!(prune_automatic(root, 1_000).unwrap().size, 210);
        fs::write(root.join("locks/.size"), b"corrupt").unwrap();
        assert_eq!(prune_automatic(root, 1_000).unwrap().size, 210);
        let mut memo: SizeMemo =
            serde_json::from_slice(&fs::read(root.join("locks/.size")).unwrap()).unwrap();
        memo.measured = SystemTime::UNIX_EPOCH;
        memo.size = 0;
        fs::write(root.join("locks/.size"), serde_json::to_vec(&memo).unwrap()).unwrap();
        assert_eq!(prune_automatic(root, 1_000).unwrap().size, 210);

        // An atomic publication invalidates accounting immediately.
        let temp = root.join("entries/new.tmp");
        fs::write(&temp, [0; 30]).unwrap();
        fs::rename(temp, root.join("entries/new.json")).unwrap();
        assert_eq!(prune_automatic(root, 1_000).unwrap().size, 240);
        fs::create_dir(root.join("warm")).unwrap();
        fs::write(root.join("warm/a.json"), [0; 40]).unwrap();
        assert_eq!(prune_automatic(root, 1_000).unwrap().size, 280);
        std::thread::sleep(STAMP_SETTLE + Duration::from_millis(100));
        assert_eq!(prune_automatic(root, 1_000).unwrap().size, 280);
        assert!(root.join("locks/.size").exists());
        // A lower limit must still trigger eviction with an unchanged memo.
        let result = prune_automatic(root, 250).unwrap();
        assert!(result.entries > 0);
        assert!(result.size <= 250);
    }

    #[test]
    fn old_prune_marker_forces_full_reconciliation() {
        let root = fixture();
        let root = root.path();
        let marker = File::options()
            .write(true)
            .open(root.join("locks/.prune"))
            .unwrap();
        marker.set_modified(SystemTime::UNIX_EPOCH).unwrap();
        let result = prune_automatic(root, 100).unwrap();
        assert_eq!(result.entries, 1);
        assert_eq!(result.size, 100);
    }

    #[test]
    fn parses_sizes_like_nx() {
        assert_eq!(parse_size("1024").unwrap(), 1024);
        assert_eq!(parse_size("1KB").unwrap(), 1024);
        assert_eq!(parse_size("1.5 MB").unwrap(), 1_572_864);
        assert_eq!(parse_size("2GB").unwrap(), 2 << 30);
        assert_eq!(parse_size("0").unwrap(), 0);
        for bad in ["", "1TB", "1.2.3MB", "MB", "-1"] {
            assert!(parse_size(bad).is_err(), "{bad}");
        }
    }
}
