//! The remote store: what to fetch, upload and list, over a [`store::Store`]
//! that moves the bytes. Today that is S3-compatible storage ([`s3`]),
//! configured by nx.json's `s3` key as `@nx/s3-cache` reads it.
//!
//! Objects live under `<cacheKeyPrefix>qk/v3/`, so a bucket shared with Nx never
//! mixes the two: `entries/<key>` for results and `warm/<task>/<branch>` for
//! warm records. Each object is a Zstandard level 3 pack holding a manifest or
//! record with every blob it cites, so one request reads or writes it whole;
//! the store's
//! latency is per request, and an entry can cite thousands of blobs. A local
//! miss fetches the pack and keeps the blobs missing locally, verifying each
//! against its hash; the restore then runs locally. Uploads run in the
//! background after a task is saved. An [`index`] of the entries the store
//! holds spares the lookups it would answer with a miss.

pub(crate) mod background;
mod index;
mod pack;
mod s3;
mod store;

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::Value;

use store::{Object, Store};

/// Uploads in flight at once.
const UPLOADERS: usize = 8;
/// Connections kept open between requests to the store.
const IDLE_CONNECTIONS: usize = 32;
/// The most a single upload may hold, as S3 allows.
const LARGEST_UPLOAD: u64 = 5 << 30;
/// The most a reader of the index keeps of all its listings together.
const LARGEST_INDEX: usize = 32 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    ReadWrite,
    Read,
}

pub struct Remote {
    store: Box<dyn Store>,
    pub mode: Mode,
    uploads: Mutex<Option<Uploads>>,
    background: Option<background::Configuration>,
    pending: Mutex<Vec<Upload>>,
    /// What the store's index says it holds, once read: `None` when it could
    /// not be read, and every lookup then goes to the store.
    known: OnceLock<Option<index::Known>>,
    /// The index being read in the background.
    reading: Mutex<Option<JoinHandle<Option<index::Known>>>>,
    /// Entries this run found in the store or put there, for its listing.
    confirmed: Mutex<BTreeSet<String>>,
    /// Write-ahead listings whose entries this run confirmed. They can be
    /// removed only after a final listing has safely replaced them.
    announcements: Mutex<BTreeSet<String>>,
    /// Entries this run found in the local cache, which its listing names
    /// too when the index already does.
    held: Mutex<BTreeSet<String>>,
}

struct Uploads {
    sender: Sender<Upload>,
    workers: Vec<JoinHandle<()>>,
    failures: Arc<Mutex<Vec<String>>>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Upload {
    /// The entry's key, or the warm record's task, for reporting failures.
    key: String,
    local: PathBuf,
    /// A warm record to write, with its task identity and branch, instead of
    /// the entry.
    warm: Option<Warm>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Warm {
    identity: String,
    branch: String,
    record: Vec<u8>,
}

/// An HTTP client for a store: statuses are answers, not errors.
fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_global(Some(Duration::from_secs(10 * 60)))
        // Fetches run in every task slot at once, beside the uploaders, all to
        // one host. Keeping that many connections idle lets a burst reuse
        // them, where ureq's default of three would make most of it open a
        // new connection and repeat the TLS handshake.
        .max_idle_connections(IDLE_CONNECTIONS)
        .max_idle_connections_per_host(IDLE_CONNECTIONS)
        .build()
        .into()
}

/// The remote store for a workspace, `Ok(None)` when none is configured or the
/// mode turns it off, or why a configured store cannot be used.
pub fn configure(
    config: Option<&Value>,
    environment: &BTreeMap<OsString, OsString>,
) -> Result<Option<Remote>> {
    let Some(config) = config else {
        return Ok(None);
    };
    let variable = |name: &str| {
        environment
            .get(std::ffi::OsStr::new(name))
            .and_then(|value| value.to_str())
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let setting = |name: &str| config.get(name).and_then(Value::as_str).map(str::to_owned);
    // `--skip-remote-cache` sets the first, as in Nx.
    if ["NX_SKIP_REMOTE_CACHE", "NX_DISABLE_REMOTE_CACHE"]
        .iter()
        .any(|name| variable(name).as_deref() == Some("true"))
    {
        return Ok(None);
    }
    let ci = variable("CI").is_some_and(|value| value != "false" && value != "0");
    let mode = variable("NX_POWERPACK_CACHE_MODE")
        .or_else(|| setting(if ci { "ciMode" } else { "localMode" }))
        .unwrap_or_else(|| "read-write".into());
    let mode = match mode.as_str() {
        "read-write" => Mode::ReadWrite,
        "read" | "read-only" => Mode::Read,
        "no-cache" => return Ok(None),
        other => bail!("unknown remote cache mode {other:?}; use read-write, read or no-cache"),
    };
    let store = s3::configure(config, variable)?;
    let upload_mode = variable("QK_REMOTE_UPLOAD_MODE")
        .or_else(|| setting("uploadMode"))
        .unwrap_or_else(|| "wait".into());
    let background = match upload_mode.as_str() {
        "wait" => None,
        "background" => {
            let credentials = &store.credentials;
            let mut config = config.clone();
            config["accessKeyId"] = credentials.key().into();
            config["secretAccessKey"] = credentials.secret().into();
            config["uploadMode"] = "wait".into();
            Some(background::Configuration {
                config,
                token: credentials.token().map(str::to_owned),
            })
        }
        other => bail!("unknown s3 upload mode {other:?}; use wait or background"),
    };
    Ok(Some(Remote {
        store: Box::new(store),
        mode,
        uploads: Mutex::new(None),
        background,
        pending: Mutex::new(Vec::new()),
        known: OnceLock::new(),
        reading: Mutex::new(None),
        confirmed: Mutex::default(),
        announcements: Mutex::default(),
        held: Mutex::default(),
    }))
}

impl Remote {
    /// Starts reading the store's index in the background, keeping a copy in
    /// the local cache at `root` so that later runs fetch only what is new.
    pub(crate) fn read_index(self: &Arc<Self>, root: &Path) {
        let path = root
            .join("remote")
            .join(&blake3::hash(self.store.location().as_bytes()).to_hex()[..32]);
        let remote = self.clone();
        let reading = std::thread::spawn(move || match remote.sync_index(&path) {
            Ok(known) => Some(known),
            Err(error) => {
                qk_executor::status!(
                    "qk: remote cache index unavailable, looking up each entry ({error:#})"
                );
                None
            }
        });
        *self.reading.lock().unwrap() = Some(reading);
    }

    /// The store's index, waiting for it to be read.
    fn known(&self) -> Option<&index::Known> {
        self.known
            .get_or_init(|| {
                let reading = self.reading.lock().unwrap().take()?;
                reading.join().ok().flatten()
            })
            .as_ref()
    }

    /// The local copy at `path` brought up to date with the index the store
    /// holds now.
    fn sync_index(&self, path: &Path) -> Result<index::Known> {
        let mut known = index::Known::load(path);
        self.store.sync_index(&mut known)?;
        known.expire(index::now());
        if let Err(error) = known.save(path) {
            qk_executor::status!("qk: could not save the remote cache index ({error:#})");
        }
        Ok(known)
    }

    /// Copies the entry for `key` into the local store at `root`, returning
    /// whether the remote had it. Every blob the local store lacks is verified
    /// before it is kept.
    pub(crate) fn fetch(&self, root: &Path, key: &str, task: &str) -> Result<bool> {
        let waiting = crate::profile::span(task, "remote_index_wait");
        let known = self.known();
        drop(waiting);
        if known.is_some_and(|known| !known.keys.contains_key(key)) {
            return Ok(false);
        }
        let Some(manifest) = self.fetch_pack(root, Object::Entry(key), task, |parsed| {
            parsed.check_key(key)
        })?
        else {
            return Ok(false);
        };
        crate::record::publish_bytes(
            root,
            &root.join("entries").join(format!("{key}.json")),
            &manifest,
        )?;
        self.confirmed.lock().unwrap().insert(key.to_owned());
        Ok(true)
    }

    /// Notes that the local cache held the entry for `key`.
    pub(crate) fn held(&self, key: &str) {
        self.held.lock().unwrap().insert(key.to_owned());
    }

    /// Queues the local entry for `key` for upload.
    pub(crate) fn upload(self: &Arc<Self>, root: &Path, key: &str) {
        self.enqueue(Upload {
            key: key.to_owned(),
            local: root.to_owned(),
            warm: None,
        });
    }

    /// Queues a task's warm record for its branch, after the blobs it cites.
    pub(crate) fn upload_warm(
        self: &Arc<Self>,
        root: &Path,
        task: &str,
        branch: &str,
        record: Vec<u8>,
    ) {
        self.enqueue(Upload {
            key: format!("{task} warm state"),
            local: root.to_owned(),
            warm: Some(Warm {
                identity: task.to_owned(),
                branch: branch.to_owned(),
                record,
            }),
        });
    }

    /// The task's warm record saved on `branch`, with every blob it cites
    /// fetched into the local store and verified, or `None` when there is
    /// none or `accept` rejects its key. The key is checked before any blob is
    /// read, so a save that does not suit this checkout costs only its record.
    pub(crate) fn fetch_warm(
        &self,
        root: &Path,
        identity: &str,
        task: &str,
        branch: &str,
        accept: impl Fn(&[String]) -> bool,
    ) -> Result<Option<Vec<u8>>> {
        let fetched = self.fetch_pack(root, Object::Warm { identity, branch }, task, |record| {
            if record.warm_key().is_some_and(|key| accept(&key)) {
                Ok(())
            } else {
                Err(OtherWarmKey.into())
            }
        });
        match fetched {
            Err(error) if error.is::<OtherWarmKey>() => Ok(None),
            fetched => fetched,
        }
    }

    /// The record in the pack at `object`, once `check` accepts it and the
    /// blobs it cites that the local store lacks are kept there, or `None`
    /// when there is no such object. `task` names the task the fetch is for
    /// in profile timings.
    fn fetch_pack(
        &self,
        root: &Path,
        object: Object,
        task: &str,
        check: impl Fn(&crate::record::StoredRecord) -> Result<()>,
    ) -> Result<Option<Vec<u8>>> {
        let Some(body) = self.store.get(object, task)? else {
            return Ok(None);
        };
        let mut receiving = crate::profile::span(task, "remote_body");
        let mut transfer = pack::Transfer::default();
        let record = pack::read(root, body, check, &mut transfer);
        receiving.count("bytes", transfer.received);
        receiving.count("held_bytes", transfer.held);
        record.map(Some)
    }

    fn enqueue(self: &Arc<Self>, upload: Upload) {
        if self.mode != Mode::ReadWrite {
            return;
        }
        if self.background.is_some() {
            self.pending.lock().unwrap().push(upload);
            return;
        }
        let mut uploads = self.uploads.lock().unwrap();
        let uploads = uploads.get_or_insert_with(|| {
            let (sender, receiver) = channel::<Upload>();
            let receiver = Arc::new(Mutex::new(receiver));
            let failures = Arc::new(Mutex::new(Vec::new()));
            let workers = (0..UPLOADERS)
                .map(|_| {
                    let remote = self.clone();
                    let receiver: Arc<Mutex<Receiver<Upload>>> = receiver.clone();
                    let failures = failures.clone();
                    std::thread::spawn(move || {
                        loop {
                            let job = receiver.lock().unwrap().recv();
                            let Ok(job) = job else { return };
                            if let Err(error) = remote.send(&job) {
                                failures
                                    .lock()
                                    .unwrap()
                                    .push(format!("{}: {error:#}", job.key));
                            }
                        }
                    })
                })
                .collect();
            Uploads {
                sender,
                workers,
                failures,
            }
        });
        let _ = uploads.sender.send(upload);
    }

    fn send(&self, job: &Upload) -> Result<()> {
        if let Some(warm) = &job.warm {
            // The newest save for the branch replaces the last one.
            let object = Object::Warm {
                identity: &warm.identity,
                branch: &warm.branch,
            };
            return self.put_pack(&job.local, object, &warm.record);
        }
        let object = Object::Entry(&job.key);
        let exists = self.store.has(object)?;
        let announcement = self.store.announce(&job.key)?;
        if !exists {
            let manifest = fs::read(job.local.join("entries").join(format!("{}.json", job.key)))?;
            self.put_pack(&job.local, object, &manifest)?;
        }
        self.confirmed.lock().unwrap().insert(job.key.clone());
        self.announcements.lock().unwrap().extend(announcement);
        Ok(())
    }

    /// Uploads `record` with the blobs it cites from the local store at
    /// `root`, as one pack.
    fn put_pack(&self, root: &Path, object: Object, record: &[u8]) -> Result<()> {
        let mut pack = tempfile::NamedTempFile::new_in(root.join("tmp"))?;
        pack::write(root, record, pack.as_file_mut())?;
        let file = pack.reopen()?;
        if file.metadata()?.len() > LARGEST_UPLOAD {
            bail!("too large to upload as one object");
        }
        self.store.put(object, file)
    }

    /// Waits for queued uploads, then adds this run's listing to the index,
    /// returning what failed.
    pub(crate) fn finish(&self) -> Vec<String> {
        if let Some(configuration) = &self.background {
            let pending = std::mem::take(&mut *self.pending.lock().unwrap());
            if !pending.is_empty() {
                match background::launch(
                    configuration,
                    &pending,
                    self.held.lock().unwrap().iter().cloned().collect(),
                    self.confirmed.lock().unwrap().iter().cloned().collect(),
                ) {
                    Ok(log) => {
                        qk_executor::status!(
                            "qk: remote uploads continuing in background; log: {}",
                            log.display()
                        );
                        return Vec::new();
                    }
                    Err(error) => {
                        qk_executor::status!(
                            "qk: could not detach remote uploads; waiting instead ({error:#})"
                        );
                        let mut failures = Vec::new();
                        for job in pending {
                            if let Err(error) = self.send(&job) {
                                failures.push(format!("{}: {error:#}", job.key));
                            }
                        }
                        if let Err(error) = self.add_listing() {
                            failures.push(format!("the index: {error:#}"));
                        }
                        return failures;
                    }
                }
            }
        }
        let mut failures = Vec::new();
        if let Some(uploads) = self.uploads.lock().unwrap().take() {
            drop(uploads.sender);
            for worker in uploads.workers {
                let _ = worker.join();
            }
            failures = std::mem::take(&mut *uploads.failures.lock().unwrap());
        }
        if self.mode == Mode::ReadWrite
            && let Err(error) = self.add_listing()
        {
            failures.push(format!("the index: {error:#}"));
        }
        failures
    }

    /// Lists the entries this run found in the store or put there.
    fn add_listing(&self) -> Result<()> {
        let mut confirmed = std::mem::take(&mut *self.confirmed.lock().unwrap());
        let held = std::mem::take(&mut *self.held.lock().unwrap());
        if confirmed.is_empty() && held.is_empty() {
            return Ok(());
        }
        let known = self.known();
        if let Some(known) = known {
            confirmed.extend(held.into_iter().filter(|key| known.keys.contains_key(key)));
        }
        // Local-only runs renew entries the store already lists, but never
        // advertise a local result that has not been confirmed remotely.
        if confirmed.is_empty() {
            return Ok(());
        }
        let announcements = std::mem::take(&mut *self.announcements.lock().unwrap());
        self.store.list(confirmed, known, announcements)
    }
}

/// Read bounded index payloads without trusting Content-Length. Each reader
/// charges the shared budget before retaining bytes; failed reads remain
/// charged so parallel failures cannot exceed the synchronization budget.
fn read_bounded(
    mut reader: impl Read,
    limit: usize,
    budget: Option<&AtomicUsize>,
) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    let mut chunk = [0u8; 8_192];
    loop {
        let count = reader.read(&mut chunk)?;
        if count == 0 {
            return Ok(body);
        }
        if count > limit.saturating_sub(body.len()) {
            bail!("remote index listing exceeds its {limit}-byte limit");
        }
        if let Some(budget) = budget {
            let mut used = budget.load(Ordering::Relaxed);
            loop {
                if count > LARGEST_INDEX.saturating_sub(used) {
                    bail!("remote index exceeds its {LARGEST_INDEX}-byte read budget");
                }
                match budget.compare_exchange_weak(
                    used,
                    used + count,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(current) => used = current,
                }
            }
        }
        body.extend_from_slice(&chunk[..count]);
    }
}

#[cfg(test)]
mod index_read_tests {
    use super::{LARGEST_INDEX, read_bounded};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn listing_limits_apply_without_length_headers() {
        assert_eq!(read_bounded(&b"abcd"[..], 4, None).unwrap(), b"abcd");
        assert!(read_bounded(&b"abcde"[..], 4, None).is_err());
    }

    #[test]
    fn listing_readers_share_one_payload_budget() {
        let used = AtomicUsize::new(LARGEST_INDEX - 4);
        std::thread::scope(|scope| {
            let used = &used;
            let a = scope.spawn(|| read_bounded(&b"abcd"[..], 4, Some(used)));
            let b = scope.spawn(|| read_bounded(&b"abcd"[..], 4, Some(used)));
            let results = [a.join().unwrap(), b.join().unwrap()];
            assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
            assert_eq!(results.iter().filter(|r| r.is_err()).count(), 1);
        });
        assert_eq!(used.load(Ordering::Relaxed), LARGEST_INDEX);
    }
}

/// A remote warm save whose key does not suit this checkout.
#[derive(Debug)]
struct OtherWarmKey;

impl std::fmt::Display for OtherWarmKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("remote warm state is for another key")
    }
}

impl std::error::Error for OtherWarmKey {}
