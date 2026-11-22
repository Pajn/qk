//! Keeps the cache under its size limit by evicting the least recently used
//! entries, as Nx's `maxCacheSize` does: once over the limit, down to 90% of
//! it. As in Nx, entries unused for a week go whatever the limit.
//!
//! An entry's last use is its manifest's modification time, refreshed on every
//! hit. Blobs are shared between entries, so a blob is deleted only once no
//! remaining manifest cites it. A blob no manifest cites may belong to an entry
//! another run is still publishing, so it is kept until it is an hour old.
//! Blobs a fetch relies on are [pinned](pin) until it publishes its record,
//! and kept whatever their age or how many manifests cite them.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How long an unreferenced blob or scratch file may be part of a publish.
const IN_FLIGHT: Duration = Duration::from_secs(60 * 60);

/// How long an entry may go unused before it is evicted, as Nx's
/// `remove_old_cache_records` evicts records not accessed in 7 days.
const UNUSED: Duration = Duration::from_secs(7 * 24 * 60 * 60);

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

/// The size limit: `QK_MAX_CACHE_SIZE`, qk's alone, then `NX_MAX_CACHE_SIZE`,
/// then nx.json `maxCacheSize`, then, as in Nx, a tenth of the disk holding the
/// cache, or 100GB when the disk's size cannot be read. `None` when the limit
/// is 0, which means unlimited.
pub fn max_size(configured: Option<&Value>, cache: &Path) -> Result<Option<u64>> {
    let configured =
        match std::env::var("QK_MAX_CACHE_SIZE").or_else(|_| std::env::var("NX_MAX_CACHE_SIZE")) {
            Ok(value) => Some(value),
            Err(_) => configured.map(|value| match value {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            }),
        };
    let limit = match configured {
        Some(text) => parse_size(&text)?,
        None => cache
            .ancestors()
            .find(|path| path.exists())
            .and_then(|existing| fs2::total_space(existing).ok())
            .map_or(100 << 30, |total| total / 10),
    };
    Ok((limit > 0).then_some(limit))
}

/// Where pins live: under `locks/`, whose dot-named items are not lock files.
fn pins(root: &Path) -> PathBuf {
    root.join("locks").join(".pins")
}

/// Held shared while a pin is made, and exclusively by a prune while it
/// deletes, so a pin made while a prune runs waits until it is done.
fn pin_gate(root: &Path) -> Result<File> {
    let directory = pins(root);
    fs::create_dir_all(&directory)?;
    Ok(File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(directory.join(".gate"))?)
}

/// Blobs kept from eviction while this is alive. Each pin is two files: one
/// its holder keeps locked, so a prune can tell a live pin from an abandoned
/// one, and one naming the blobs, which a prune can read even where a lock
/// keeps other processes from reading the file it covers.
#[derive(Debug)]
pub(crate) struct Pin {
    lock: Option<File>,
    path: PathBuf,
}

/// Keeps `blobs` from being pruned until the returned [`Pin`] is dropped. A
/// prune already deleting finishes first, so a caller that checks which blobs
/// exist after pinning sees every deletion.
pub(crate) fn pin(root: &Path, blobs: &BTreeSet<String>) -> Result<Pin> {
    let gate = pin_gate(root)?;
    gate.lock_shared()?;
    let directory = pins(root);
    let name = format!(
        "{}-{}",
        std::process::id(),
        &blake3::hash(
            format!("{:?}{:?}", std::thread::current().id(), SystemTime::now()).as_bytes()
        )
        .to_hex()[..16]
    );
    let path = directory.join(name);
    let lock = File::options()
        .create_new(true)
        .write(true)
        .open(path.with_extension("lock"))?;
    lock.lock_exclusive()?;
    let pin = Pin {
        lock: Some(lock),
        path,
    };
    let mut listing = String::new();
    for blob in blobs {
        listing.push_str(blob);
        listing.push('\n');
    }
    let mut file = File::create(pin.path.with_extension("blobs"))?;
    file.write_all(listing.as_bytes())?;
    drop(gate);
    Ok(pin)
}

impl Drop for Pin {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.path.with_extension("blobs"));
        // Unlock last: a prune that finds the lock free removes what is left.
        drop(self.lock.take());
        let _ = fs::remove_file(self.path.with_extension("lock"));
    }
}

/// The blobs live pins hold, removing pins whose holder is gone.
fn pinned(root: &Path) -> BTreeSet<String> {
    let mut held = BTreeSet::new();
    for item in fs::read_dir(pins(root)).into_iter().flatten().flatten() {
        let path = item.path();
        if path.extension().is_none_or(|extension| extension != "lock") {
            continue;
        }
        let Ok(lock) = File::open(&path) else {
            continue;
        };
        if lock.try_lock_exclusive().is_ok() {
            let _ = fs::remove_file(path.with_extension("blobs"));
            let _ = fs::remove_file(&path);
            continue;
        }
        if let Ok(listing) = File::open(path.with_extension("blobs")) {
            held.extend(
                std::io::BufReader::new(listing)
                    .lines()
                    .map_while(Result::ok),
            );
        }
    }
    held
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

/// Evicts entries unused for a week and, when the cache holds more than
/// `limit` bytes, least recently used entries until it holds at most 90% of
/// it, and removes what no entry needs. Another prune in progress makes
/// this one return without doing anything.
///
/// Explicit pruning always remeasures the cache, including files changed in
/// place by external tools. Normal runs use [`prune_automatic`].
pub fn prune(root: &Path, limit: u64) -> Result<Pruned> {
    prune_inner(root, limit, false)
}

/// Enforces the limit after a run. Within an hour of the last full prune, and
/// while the cache's [ledger](Ledger) puts it within the limit, it reads that
/// size rather than measuring: publishing reports what it adds, so a run that
/// saved something does not stat the whole cache to learn so. Otherwise, as
/// when the ledger's size is over the limit, it prunes in full, which counts
/// the cache exactly.
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
    // What this process wrote and has not reported yet counts too.
    grew(root, 0);
    if automatic && recent {
        let now = SystemTime::now();
        // A ledger that cannot be read, or written where it is missing, as
        // on a full disk, says nothing: the cache is counted and pruned.
        if let Ok(Some(size)) = with_ledger(root, |ledger| ledger.size(now))
            && size <= limit
        {
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
    let _ = guard.set_modified(SystemTime::now());
    // Where the ledger stood before any file is looked at: a publisher that
    // reports after this point may be counted twice, which is safe, and one
    // that reported before it had its files in place, so the count sees them.
    // A ledger that cannot be read or written must not keep a full disk from
    // being pruned: the count is then made and enforced, and not recorded.
    let counting_from = with_ledger(root, |ledger| ledger.added).ok();
    // Held until deletion ends, so no pin is made while blobs are deleted.
    let gate = pin_gate(root)?;
    gate.lock_exclusive()?;
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
    // Path sets and projection manifests cite none, but take space and age
    // out like entries.
    let warm = fs::read_dir(root.join("warm")).into_iter().flatten();
    let pathsets = fs::read_dir(root.join("pathsets")).into_iter().flatten();
    let projections = fs::read_dir(root.join("projections")).into_iter().flatten();
    let items = fs::read_dir(root.join("entries"))?
        .chain(warm)
        .chain(pathsets)
        .chain(projections)
        .collect::<std::io::Result<Vec<_>>>()?;
    let read = in_parallel(items, |item| -> Result<Entry> {
        let metadata = item.metadata()?;
        // Unreadable manifests are misses already; they only take space.
        let blobs = fs::read(item.path())
            .ok()
            .and_then(|bytes| crate::record::StoredRecord::read(&bytes).ok())
            .map(|record| record.blobs())
            .unwrap_or_default();
        Ok(Entry {
            path: item.path(),
            used: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            size: metadata.len(),
            blobs,
        })
    });
    for entry in read {
        let entry = entry?;
        for blob in &entry.blobs {
            *references.entry(blob.clone()).or_default() += 1;
        }
        entries.push(entry);
    }
    // A pin is a reference no eviction removes.
    for blob in pinned(root) {
        *references.entry(blob).or_default() += 1;
    }
    let mut blob_sizes = BTreeMap::new();
    let mut pruned = Pruned::default();
    let blobs = fs::read_dir(root.join("blobs"))?.collect::<std::io::Result<Vec<_>>>()?;
    let blobs = in_parallel(blobs, |item| {
        let metadata = item.metadata()?;
        Ok::<_, std::io::Error>((item, metadata))
    });
    for blob in blobs {
        let (item, metadata) = blob?;
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
    // Nx stops a tenth below the limit, so the next run does not evict again.
    let target = if size > limit {
        limit - limit / 10
    } else {
        limit
    };
    for entry in entries {
        let unused = now.duration_since(entry.used).is_ok_and(|age| age > UNUSED);
        if size <= target && !unused {
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
    drop(gate);
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
    // Eviction's deletions are in `size`; what was published meanwhile is in
    // the ledger after `counting_from`. Without that, there is nothing to
    // count on from, and the count an earlier prune recorded stands: it
    // leaves out only what this one deleted.
    if let Some(added) = counting_from {
        let _ = with_ledger(root, |ledger| {
            ledger.counted = Some(Counted {
                at: now,
                size,
                added,
            });
        });
    }
    pruned.size = size;
    Ok(pruned)
}

/// Where the size ledger lives: under `locks/`, whose dot-named items are
/// not lock files.
fn ledger_path(root: &Path) -> PathBuf {
    root.join("locks").join(".ledger")
}

/// What the cache's size is known to be without measuring it: the last full
/// count, plus every byte reported added since it began.
///
/// `added` only grows. A count notes where `added` stood before it looked at
/// any file, so a file published while it ran is either counted or reported
/// after that point, or both, and never neither: the size the ledger gives
/// can be too large, never too small, for writes that report. Too large only
/// brings the next full count forward. Deletions are not reported, nor are
/// changes made outside qk; the full count, at least hourly, corrects both.
#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Ledger {
    added: u64,
    counted: Option<Counted>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Counted {
    /// When the count began.
    at: SystemTime,
    size: u64,
    /// `added` when the count began.
    added: u64,
}

impl Ledger {
    /// The cache's size, unless the last count is missing or over an hour old,
    /// or `added` went back, as it does when the ledger was lost and remade.
    fn size(&self, now: SystemTime) -> Option<u64> {
        let counted = self.counted.as_ref()?;
        let fresh = now
            .duration_since(counted.at)
            .is_ok_and(|age| age < IN_FLIGHT);
        let since = self.added.checked_sub(counted.added)?;
        fresh.then(|| counted.size.saturating_add(since))
    }
}

/// Runs `change` on the ledger with it locked, and writes back what it leaves.
/// An unreadable ledger reads as empty, which has no count and so sends the
/// next automatic prune to a full one.
fn with_ledger<T>(root: &Path, change: impl FnOnce(&mut Ledger) -> T) -> Result<T> {
    use std::io::{Read, Seek};
    let mut file = File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(ledger_path(root))?;
    // Read and written through the locked handle, which Windows also allows.
    file.lock_exclusive()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let mut ledger: Ledger = serde_json::from_slice(&bytes).unwrap_or_default();
    let result = change(&mut ledger);
    let after = serde_json::to_vec(&ledger)?;
    if after != bytes {
        file.rewind()?;
        file.set_len(0)?;
        file.write_all(&after)?;
    }
    Ok(result)
}

/// Bytes this process put in caches and has not yet entered in their ledger,
/// by cache root. Blobs are noted here as they are written, often many at
/// once on several threads, and entered with the manifest that cites them,
/// so a save locks the ledger once rather than once a blob.
static UNREPORTED: std::sync::Mutex<BTreeMap<PathBuf, u64>> =
    std::sync::Mutex::new(BTreeMap::new());

/// Notes that `bytes` were just put in the cache at `root`, in a blob already
/// in place. They reach the ledger with the next [`grew`] or prune.
pub(crate) fn noted(root: &Path, bytes: u64) {
    let mut unreported = UNREPORTED.lock().unwrap_or_else(|error| error.into_inner());
    *unreported.entry(root.to_owned()).or_default() += bytes;
}

/// Enters `bytes`, just put in the cache at `root` and already in place, in
/// its ledger, with everything [`noted`] holds, for whichever cache it was
/// noted. Call it after the files are in place, never before, so a count that
/// began earlier sees them or this. A failure loses only accuracy, which the
/// next full count restores.
pub(crate) fn grew(root: &Path, bytes: u64) {
    // Held while the ledgers are written, so a prune on another thread never
    // finds bytes taken from here and not yet in a ledger.
    let mut unreported = UNREPORTED.lock().unwrap_or_else(|error| error.into_inner());
    *unreported.entry(root.to_owned()).or_default() += bytes;
    for (root, bytes) in std::mem::take(&mut *unreported) {
        if bytes > 0 {
            let _ = with_ledger(&root, |ledger| {
                ledger.added = ledger.added.saturating_add(bytes)
            });
        }
    }
}

/// `each` of `items`, in order, on a few threads once there are enough of
/// them: one after another, stat-ing and reading a cache of tens of thousands
/// of files takes most of a second.
fn in_parallel<T: Send, R: Send>(items: Vec<T>, each: impl Fn(T) -> R + Sync) -> Vec<R> {
    let workers = items.len().div_ceil(1024).clamp(1, 8);
    if workers == 1 {
        return items.into_iter().map(each).collect();
    }
    let chunk = items.len().div_ceil(workers);
    let mut items = items.into_iter();
    let chunks: Vec<Vec<T>> = (0..workers)
        .map(|_| items.by_ref().take(chunk).collect())
        .collect();
    let each = &each;
    std::thread::scope(|scope| {
        let handles: Vec<_> = chunks
            .into_iter()
            .map(|chunk| scope.spawn(move || chunk.into_iter().map(each).collect::<Vec<R>>()))
            .collect();
        handles
            .into_iter()
            .flat_map(|handle| handle.join().expect("cache scan panicked"))
            .collect()
    })
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

    /// A cache whose one entry cites one blob, dated two hours ago.
    fn cited() -> (tempfile::TempDir, String) {
        let root = fixture();
        let blob = "b".repeat(64);
        fs::write(root.path().join("blobs").join(&blob), [0; 100]).unwrap();
        fs::write(
            root.path().join("entries/a.json"),
            serde_json::to_vec(&serde_json::json!({"log": blob})).unwrap(),
        )
        .unwrap();
        let old = SystemTime::now() - Duration::from_secs(2 * 60 * 60);
        for path in [format!("blobs/{blob}"), "entries/a.json".into()] {
            File::options()
                .write(true)
                .open(root.path().join(path))
                .unwrap()
                .set_modified(old)
                .unwrap();
        }
        (root, blob)
    }

    /// Prunes until `blob` is gone. A prune finds `.prune` held, and does
    /// nothing, while a child another test is starting has not yet exec'd
    /// away its copy of the lock.
    fn prune_away(root: &Path, limit: u64, blob: &str) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while root.join("blobs").join(blob).exists() && std::time::Instant::now() < deadline {
            prune(root, limit).unwrap();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!root.join("blobs").join(blob).exists());
    }

    #[test]
    fn a_pinned_blob_survives_evicting_the_last_entry_that_cites_it() {
        let (root, blob) = cited();
        let root = root.path();
        let pin = pin(root, &BTreeSet::from([blob.clone()])).unwrap();
        assert_eq!(prune(root, 0).unwrap().entries, 1);
        assert!(root.join("blobs").join(&blob).exists());
        drop(pin);
        prune_away(root, 0, &blob);
    }

    #[test]
    fn a_pinned_old_blob_no_entry_cites_survives() {
        let (root, blob) = cited();
        let root = root.path();
        fs::remove_file(root.join("entries/a.json")).unwrap();
        let pin = pin(root, &BTreeSet::from([blob.clone()])).unwrap();
        prune(root, 1 << 30).unwrap();
        assert!(root.join("blobs").join(&blob).exists());
        drop(pin);
        assert!(
            fs::read_dir(pins(root))
                .unwrap()
                .all(|item| { item.unwrap().file_name() == ".gate" })
        );
        prune_away(root, 1 << 30, &blob);
    }

    #[test]
    fn an_abandoned_pin_neither_keeps_blobs_nor_stays() {
        let (root, blob) = cited();
        let root = root.path();
        fs::create_dir_all(pins(root)).unwrap();
        fs::write(pins(root).join("gone.lock"), []).unwrap();
        fs::write(pins(root).join("gone.blobs"), format!("{blob}\n")).unwrap();
        prune(root, 0).unwrap();
        assert!(!root.join("blobs").join(&blob).exists());
        assert!(!pins(root).join("gone.lock").exists());
        assert!(!pins(root).join("gone.blobs").exists());
    }

    #[test]
    fn a_pin_waits_for_a_prune_that_is_deleting() {
        let (root, blob) = cited();
        let root = root.path().to_owned();
        let gate = pin_gate(&root).unwrap();
        gate.lock_exclusive().unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        let pinning = std::thread::spawn({
            let root = root.clone();
            move || {
                let pin = pin(&root, &BTreeSet::from([blob])).unwrap();
                sender.send(()).unwrap();
                pin
            }
        });
        assert!(receiver.recv_timeout(Duration::from_millis(200)).is_err());
        drop(gate);
        receiver.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(pinning.join().unwrap());
    }

    /// `prune`, retried while another test's child holds `.prune`, which
    /// makes a full prune do nothing.
    fn retried(prune: impl Fn() -> Result<Pruned>) -> Pruned {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let pruned = prune().unwrap();
            if pruned != Pruned::default() || std::time::Instant::now() > deadline {
                return pruned;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn automatic_size(root: &Path, limit: u64) -> u64 {
        retried(|| prune_automatic(root, limit)).size
    }

    fn ledger(root: &Path) -> Ledger {
        serde_json::from_slice(&fs::read(ledger_path(root)).unwrap()).unwrap()
    }

    #[test]
    fn automatic_prunes_read_the_size_from_the_ledger() {
        let root = fixture();
        let root = root.path();
        // No count yet: the automatic prune counts in full.
        assert_eq!(automatic_size(root, 1_000), 110);
        assert_eq!(ledger(root).counted.unwrap().size, 110);

        // Files changed without reporting, as outside qk, are not seen until
        // the next full count: the automatic prune reads no file but the
        // ledger.
        fs::write(root.join("blobs/a"), [0; 200]).unwrap();
        fs::write(root.join("entries/b.json"), [0; 50]).unwrap();
        assert_eq!(prune_automatic(root, 1_000).unwrap().size, 110);

        // What publishing reports is added, blobs noted before the manifest
        // citing them included.
        noted(root, 20);
        grew(root, 10);
        assert_eq!(prune_automatic(root, 1_000).unwrap().size, 140);
        // Blobs noted with no manifest after them count from the next prune.
        noted(root, 5);
        assert_eq!(prune_automatic(root, 1_000).unwrap().size, 145);

        // An explicit prune counts exactly, and the next automatic one starts
        // from that.
        assert_eq!(retried(|| prune(root, 1_000)).size, 260);
        assert_eq!(prune_automatic(root, 1_000).unwrap().size, 260);

        // A ledger over the limit sends the prune to a full count, which
        // finds the cache within it and evicts nothing.
        grew(root, 800);
        let pruned = retried(|| prune_automatic(root, 1_000));
        assert_eq!((pruned.size, pruned.entries), (260, 0));
        assert_eq!(prune_automatic(root, 1_000).unwrap().size, 260);

        // A lower limit still evicts.
        let pruned = retried(|| prune_automatic(root, 200));
        assert!(pruned.entries > 0);
        assert!(pruned.size <= 200);
    }

    #[test]
    fn an_unusable_ledger_sends_an_automatic_prune_to_a_full_count() {
        let root = fixture();
        let root = root.path();
        assert_eq!(automatic_size(root, 1_000), 110);
        fs::write(root.join("blobs/c"), [0; 40]).unwrap();
        for unusable in [
            // Unreadable.
            b"corrupt".to_vec(),
            // Counted over an hour ago.
            serde_json::to_vec(&Ledger {
                added: 0,
                counted: Some(Counted {
                    at: SystemTime::now() - IN_FLIGHT,
                    size: 0,
                    added: 0,
                }),
            })
            .unwrap(),
            // Remade after it was lost: added went back.
            serde_json::to_vec(&Ledger {
                added: 1,
                counted: Some(Counted {
                    at: SystemTime::now(),
                    size: 0,
                    added: 2,
                }),
            })
            .unwrap(),
        ] {
            fs::write(ledger_path(root), unusable).unwrap();
            assert_eq!(automatic_size(root, 1_000), 150);
        }
    }

    /// A ledger that cannot be opened, as one that cannot be made on a full
    /// disk, keeps no prune from counting the cache and evicting from it.
    #[test]
    fn a_ledger_that_cannot_be_opened_does_not_stop_a_prune() {
        let root = fixture();
        let root = root.path();
        fs::create_dir_all(ledger_path(root)).unwrap();
        assert!(with_ledger(root, |ledger| ledger.added).is_err());
        assert_eq!(automatic_size(root, 1_000), 110);
        assert_eq!(retried(|| prune(root, 1_000)).size, 110);
        let pruned = retried(|| prune_automatic(root, 100));
        assert!(pruned.entries > 0);
        assert!(pruned.size < 110);
    }

    #[test]
    fn what_is_published_while_a_count_runs_is_not_lost() {
        let root = fixture();
        let root = root.path();
        retried(|| prune(root, 1_000));
        // A publisher reports after the count noted where the ledger stood,
        // whether or not the count saw its file: counted twice at worst.
        let counting_from = with_ledger(root, |ledger| ledger.added).unwrap();
        fs::write(root.join("entries/b.json"), [0; 30]).unwrap();
        grew(root, 30);
        with_ledger(root, |ledger| {
            ledger.counted = Some(Counted {
                at: SystemTime::now(),
                size: 110,
                added: counting_from,
            })
        })
        .unwrap();
        assert_eq!(prune_automatic(root, 1_000).unwrap().size, 140);
    }

    #[test]
    fn publishing_a_record_reports_its_size() {
        let root = fixture();
        let root = root.path();
        fs::create_dir(root.join("tmp")).unwrap();
        retried(|| prune(root, 1_000));
        let before = ledger(root).added;
        crate::record::publish_bytes(root, &root.join("entries/b.json"), &[b' '; 25]).unwrap();
        assert_eq!(ledger(root).added - before, 25);
    }

    #[test]
    fn work_is_spread_over_threads_in_order() {
        let items: Vec<usize> = (0..5000).collect();
        assert_eq!(
            in_parallel(items.clone(), |item| item * 2),
            items.iter().map(|item| item * 2).collect::<Vec<_>>()
        );
        assert_eq!(in_parallel(vec![1], |item| item + 1), [2]);
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
    fn entries_unused_for_a_week_go_and_eviction_stops_a_tenth_below_the_limit() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        for name in ["entries", "blobs", "locks"] {
            fs::create_dir(root.join(name)).unwrap();
        }
        let now = SystemTime::now();
        for (name, age_days, size) in [
            ("stale", 8, 10),
            ("old", 3, 15),
            ("middle", 2, 100),
            ("new", 1, 180),
        ] {
            let path = root.join("entries").join(format!("{name}.json"));
            fs::write(&path, vec![0; size]).unwrap();
            File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(now - Duration::from_secs(age_days * 24 * 60 * 60))
                .unwrap();
        }
        let left = || {
            let mut names: Vec<String> = fs::read_dir(root.join("entries"))
                .unwrap()
                .map(|item| item.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        };
        // A prune that finds `.prune` held by another test's child does
        // nothing; try again until one runs.
        let prune = |limit: u64| {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                let pruned = prune(root, limit).unwrap();
                if pruned != Pruned::default() || std::time::Instant::now() > deadline {
                    return pruned;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        // Without a limit, only the week-old entry goes.
        assert_eq!(prune(u64::MAX).entries, 1);
        assert_eq!(left(), ["middle.json", "new.json", "old.json"]);
        // Over a limit of 290 bytes, eviction goes on to 261: evicting `old`
        // alone would leave 280.
        assert_eq!(prune(290).entries, 2);
        assert_eq!(left(), ["new.json"]);
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
