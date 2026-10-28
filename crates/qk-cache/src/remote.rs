//! The remote store: what to fetch, upload and list, over a [`store::Store`]
//! that moves the bytes. Today that is S3-compatible storage ([`s3`]),
//! configured by nx.json's `s3` key as `@nx/s3-cache` reads it.
//!
//! Objects live under `<cacheKeyPrefix>qk/v4/`, so a bucket shared with Nx never
//! mixes the two: `entries/<key>` for results, `warm/<task>/<branch>` for warm
//! records, `pathsets/<weak>` for recorded path sets, `expectations/<branch>`
//! for what tasks are expected to take ([`crate::expectations`]) and
//! `blobs/<hash>` for large blobs. Each entry or record is a
//! Zstandard level 3 [`pack`] holding it with the small blobs it cites, so one
//! request reads or writes it whole; the store's latency is per request, and
//! an entry can cite thousands of blobs. A blob whose compressed size reaches
//! a threshold is stored apart instead, uploaded once and fetched only by a
//! machine that lacks it. A local miss fetches the pack and the blobs missing
//! locally, verifying each against its hash; the restore then runs locally.
//! Uploads run in the background after a task is saved. An [`index`] of the
//! entries the store holds spares the lookups it would answer with a miss.

pub(crate) mod background;
mod bloom;
mod index;
mod pack;
mod prefetch;
mod s3;
mod server;
mod store;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::Value;
use ureq::unversioned::resolver::DefaultResolver;
use ureq::unversioned::transport::{
    Buffers, ConnectProxyConnector, ConnectionDetails, Connector, NextTimeout, RustlsConnector,
    TcpConnector, Transport,
};

pub(crate) use store::expectations_name;
use store::{Object, Store};

/// Uploads in flight at once.
const UPLOADERS: usize = 8;
/// Connections kept open between requests to the store.
const IDLE_CONNECTIONS: usize = 32;
/// The most a single upload may hold, as S3 allows.
const LARGEST_UPLOAD: u64 = 5 << 30;
/// The most a reader of the index keeps of all its listings together.
const LARGEST_INDEX: usize = 32 << 20;
/// Blobs stored apart one fetch requests at once.
const BLOB_FETCHERS: usize = 8;
/// Blobs stored apart requested at once by every fetch together, leaving
/// connections for entries and uploads.
const BLOB_REQUESTS: usize = 24;
/// The compressed size from which a blob is stored apart, unless
/// `QK_REMOTE_BLOB_THRESHOLD` says otherwise. Below it, a request costs more
/// than the bytes it would save. In `tools/bench/remote.mjs`, at 230 ms and
/// 200 Mbps, a cold restore with blobs from this size apart takes as long as
/// one with every blob inline, and one with the files held a quarter less. A
/// lower threshold restores as fast there, but that chain's large files are
/// all 4 MiB, so it does not show what many medium blobs per entry would cost.
const BLOB_THRESHOLD: u64 = 1 << 20;
/// The most an upload to a store server carries inline, counted raw, below the
/// 100 MiB a server accepts in one body (`tools/store-server/PROTOCOL.md`).
const UPLOAD_INLINE: u64 = 64 << 20;

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
    /// The keys this run used, in the order it first used them, for the run
    /// its listing records.
    order: Mutex<(Vec<String>, HashSet<String>)>,
    /// The compressed size from which a blob is stored apart.
    threshold: u64,
    /// Where uploads carry each blob, decided once per blob.
    placements: Mutex<HashMap<String, pack::Placement>>,
    /// Blobs stored apart that the store is known to hold.
    stored: Mutex<HashSet<String>>,
    /// The blobs each key this run used stores apart, for its listing.
    kept_apart: Mutex<HashMap<String, Vec<String>>>,
    /// Requests for blobs stored apart that may start now.
    blob_requests: Slots,
    /// Fetches in progress, by key, so a key is fetched once however many
    /// ask for it at a time.
    flights: Mutex<HashMap<String, Arc<Flight>>>,
    /// Entries fetched ahead of the tasks that need them, unless
    /// `QK_REMOTE_PREFETCH=off`.
    prefetch: prefetch::Prefetch,
    prefetching: bool,
}

/// A fetch in progress, and how it ended.
#[derive(Default)]
struct Flight {
    outcome: Mutex<Option<Outcome>>,
    done: Condvar,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    /// The entry is in the local cache.
    Published,
    /// The store does not hold it.
    Absent,
    /// The fetch failed, and whoever waited for it may try again.
    Failed,
}

/// A counting semaphore.
struct Slots {
    free: Mutex<usize>,
    freed: Condvar,
}

impl Slots {
    fn new(count: usize) -> Self {
        Self {
            free: Mutex::new(count),
            freed: Condvar::new(),
        }
    }

    fn take(&self) -> Slot<'_> {
        let mut free = self.free.lock().unwrap();
        while *free == 0 {
            free = self.freed.wait(free).unwrap();
        }
        *free -= 1;
        Slot(self)
    }
}

struct Slot<'a>(&'a Slots);

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        *self.0.free.lock().unwrap() += 1;
        self.0.freed.notify_one();
    }
}

/// A blob a pack stores apart that the store does not hold, which makes the
/// entry a miss rather than an error.
#[derive(Debug)]
struct MissingBlob(String);

impl std::fmt::Display for MissingBlob {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "remote blob {} is missing", self.0)
    }
}

impl std::error::Error for MissingBlob {}

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
    /// The path sets to write for a weak key, instead of the entry.
    #[serde(default)]
    path_sets: Option<PathSets>,
    /// Tasks' runs to add to the expectations the store holds, instead of
    /// the entry.
    #[serde(default)]
    expectations: Option<Expectations>,
    /// Where a detached worker finds the warm record or path sets, left out
    /// of its request, which would otherwise carry them inline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    record_file: Option<PathBuf>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Expectations {
    /// The branch to save for, then the one to start from when it has none.
    branches: Vec<String>,
    runs: BTreeMap<String, crate::expectations::Expectation>,
    /// When the runs ended, in milliseconds since the epoch.
    now: u64,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct PathSets {
    weak: String,
    record: Vec<u8>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Warm {
    identity: String,
    branch: String,
    record: Vec<u8>,
}

/// An HTTP client for a store: statuses are answers, not errors.
fn agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_global(Some(Duration::from_secs(10 * 60)))
        // Fetches run in every task slot at once, beside the uploaders, all to
        // one host. Keeping that many connections idle lets a burst reuse
        // them, where ureq's default of three would make most of it open a
        // new connection and repeat the TLS handshake.
        .max_idle_connections(IDLE_CONNECTIONS)
        .max_idle_connections_per_host(IDLE_CONNECTIONS)
        .build();
    ureq::Agent::with_parts(config, connector(), DefaultResolver::default())
}

/// ureq's default connections, a CONNECT proxy's included, with
/// [`Restarting`] between the socket and TLS: rustls restarts an interrupted
/// read itself, with the socket's whole timeout each time.
fn connector() -> impl Connector<Out = impl Transport> {
    ().chain(ConnectProxyConnector::default())
        .chain(TcpConnector::default())
        .chain(Restarting)
        .chain(RustlsConnector::default())
}

/// Wraps each connection so that a read a signal interrupts is made again,
/// within the time the read had left.
///
/// On Linux a socket read with a timeout, as every read of the agent's is,
/// fails with EINTR when any signal reaches the thread, even one that is then
/// ignored. A child that exits while another thread is spawning one sends such
/// a signal: spawning blocks every signal in the spawning thread, so the
/// kernel queues SIGCHLD rather than drop it and wakes a thread that does not
/// block it. qk spawns processes beside its requests, and ureq returns EINTR as
/// the request's error; writes need nothing, as `write_all` restarts them.
#[derive(Debug)]
struct Restarting;

impl<In: Transport> Connector<In> for Restarting {
    type Out = Box<dyn Transport>;

    fn connect(
        &self,
        _details: &ConnectionDetails,
        chained: Option<In>,
    ) -> std::result::Result<Option<Self::Out>, ureq::Error> {
        Ok(chained.map(|transport| {
            Box::new(Restart(Box::new(transport) as Box<dyn Transport>)) as Self::Out
        }))
    }
}

#[derive(Debug)]
struct Restart(Box<dyn Transport>);

impl Transport for Restart {
    fn buffers(&mut self) -> &mut dyn Buffers {
        self.0.buffers()
    }

    fn transmit_output(
        &mut self,
        amount: usize,
        timeout: NextTimeout,
    ) -> std::result::Result<(), ureq::Error> {
        self.0.transmit_output(amount, timeout)
    }

    fn await_input(&mut self, timeout: NextTimeout) -> std::result::Result<bool, ureq::Error> {
        let started = std::time::Instant::now();
        let mut next = timeout;
        loop {
            match self.0.await_input(next) {
                // Nothing was read, so reading again loses nothing, within
                // what is left of the read's time.
                Err(ureq::Error::Io(error)) if error.kind() == std::io::ErrorKind::Interrupted => {
                    if let ureq::unversioned::transport::time::Duration::Exact(after) =
                        timeout.after
                    {
                        let left = after.saturating_sub(started.elapsed());
                        if left.is_zero() {
                            return Err(ureq::Error::Timeout(timeout.reason));
                        }
                        next.after = ureq::unversioned::transport::time::Duration::Exact(left);
                    }
                }
                read => return read,
            }
        }
    }

    fn is_open(&mut self) -> bool {
        self.0.is_open()
    }

    fn is_tls(&self) -> bool {
        self.0.is_tls()
    }
}

/// The remote store for a workspace, `Ok(None)` when none is configured or the
/// mode turns it off, or why a configured store cannot be used. A store
/// server is used over `s3`, which can stay for Nx's own cache plugin.
pub fn configure(
    s3: Option<&Value>,
    server: Option<&Value>,
    environment: &BTreeMap<OsString, OsString>,
) -> Result<Option<Remote>> {
    let (config, kind) = match (s3, server) {
        (None, None) => return Ok(None),
        (_, Some(config)) => (config, background::Kind::Server),
        (Some(config), None) => (config, background::Kind::S3),
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
    let ci = qk_executor::is_ci_with(|name| {
        environment
            .get(std::ffi::OsStr::new(name))
            .and_then(|value| value.to_str())
            .map(str::to_owned)
    });
    let mode = variable("NX_POWERPACK_CACHE_MODE")
        .or_else(|| setting(if ci { "ciMode" } else { "localMode" }))
        .unwrap_or_else(|| "read-write".into());
    let mode = match mode.as_str() {
        "read-write" => Mode::ReadWrite,
        "read" | "read-only" => Mode::Read,
        "no-cache" => return Ok(None),
        other => bail!("unknown remote cache mode {other:?}; use read-write, read or no-cache"),
    };
    let threshold = match variable("QK_REMOTE_BLOB_THRESHOLD").as_deref() {
        None => BLOB_THRESHOLD,
        Some("off") => u64::MAX,
        Some(size) => crate::evict::parse_size(size).map_err(|_| {
            anyhow::anyhow!(
                "invalid QK_REMOTE_BLOB_THRESHOLD {size:?}; use a size such as 1MB, or off"
            )
        })?,
    };
    let upload_mode = variable("QK_REMOTE_UPLOAD_MODE")
        .or_else(|| setting("uploadMode"))
        .unwrap_or_else(|| "wait".into());
    let background = match upload_mode.as_str() {
        "wait" => false,
        "background" => true,
        other => bail!("unknown remote upload mode {other:?}; use wait or background"),
    };
    // The background worker re-enters this function with these settings, its
    // credentials included, and waits for its own uploads.
    let mut detached = config.clone();
    detached["uploadMode"] = "wait".into();
    let (store, token): (Box<dyn Store>, Option<String>) = match kind {
        background::Kind::S3 => {
            let store = s3::configure(config, variable)?;
            let credentials = &store.credentials;
            detached["accessKeyId"] = credentials.key().into();
            detached["secretAccessKey"] = credentials.secret().into();
            let token = credentials.token().map(str::to_owned);
            (Box::new(store), token)
        }
        background::Kind::Server => {
            let store = server::configure(config, variable)?;
            let token = Some(store.token.clone());
            (Box::new(store), token)
        }
    };
    let overrides = ["QK_STORE_SERVER_URL", "QK_REMOTE_BLOB_THRESHOLD"]
        .into_iter()
        .filter_map(|name| Some((name.to_owned(), variable(name)?)))
        .collect();
    let background = background.then_some(background::Configuration {
        kind,
        config: detached,
        token,
        overrides,
    });
    let mut remote = Remote::new(store, mode, background, threshold);
    remote.prefetching = variable("QK_REMOTE_PREFETCH").as_deref() != Some("off");
    Ok(Some(remote))
}

impl Remote {
    fn new(
        store: Box<dyn Store>,
        mode: Mode,
        background: Option<background::Configuration>,
        threshold: u64,
    ) -> Self {
        Remote {
            store,
            mode,
            uploads: Mutex::new(None),
            background,
            pending: Mutex::new(Vec::new()),
            known: OnceLock::new(),
            reading: Mutex::new(None),
            confirmed: Mutex::default(),
            announcements: Mutex::default(),
            held: Mutex::default(),
            order: Mutex::default(),
            threshold,
            placements: Mutex::default(),
            stored: Mutex::default(),
            kept_apart: Mutex::default(),
            blob_requests: Slots::new(BLOB_REQUESTS),
            flights: Mutex::default(),
            prefetch: prefetch::Prefetch::default(),
            prefetching: true,
        }
    }

    /// Starts reading the store's index in the background, keeping a copy in
    /// the local cache at `root` so that later runs fetch only what is new.
    pub(crate) fn read_index(self: &Arc<Self>, root: &Path) {
        self.store.begin(root);
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
    /// before it is kept. A fetch of the same key already in progress is
    /// waited for rather than repeated, until `cancelled`.
    pub(crate) fn fetch(
        self: &Arc<Self>,
        root: &Path,
        key: &str,
        task: &str,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> Result<bool> {
        let joined = {
            let mut flights = self.flights.lock().unwrap();
            match flights.get(key) {
                Some(flight) => Some(flight.clone()),
                None => {
                    flights.insert(key.to_owned(), Arc::default());
                    None
                }
            }
        };
        if let Some(flight) = joined {
            let _waiting = crate::profile::span(task, "remote_flight_wait");
            match wait(&flight, cancelled) {
                Some(Outcome::Published) => {
                    self.prefetch.claim(key);
                    self.confirm(key);
                    return Ok(true);
                }
                Some(Outcome::Absent) | None => return Ok(false),
                // Try once more, as this fetch's own.
                Some(Outcome::Failed) => {
                    self.flights
                        .lock()
                        .unwrap()
                        .entry(key.to_owned())
                        .or_default();
                }
            }
        }
        let fetched = self.fetch_entry(root, key, task, &AtomicU64::new(0));
        self.land(key, &fetched);
        if let Ok(true) = fetched {
            self.confirm(key);
            if self.prefetching {
                self.prefetch.follow(self, root, key);
            }
        }
        fetched
    }

    /// The record of the entry for `key`, without the blobs it cites, or
    /// `None` when the store holds no such entry.
    pub(crate) fn fetch_record(&self, key: &str, task: &str) -> Result<Option<Vec<u8>>> {
        let Some(body) = self.store.get(Object::Entry(key), task)? else {
            return Ok(None);
        };
        let record = pack::read_record(body)?;
        crate::record::StoredRecord::read(&record)?.check_key(key)?;
        Ok(Some(record))
    }

    /// Fetches the entry for `key` ahead of the task that needs it, unless it
    /// is being fetched already, returning whether it was fetched.
    fn fetch_ahead(&self, root: &Path, key: &str) -> bool {
        {
            let mut flights = self.flights.lock().unwrap();
            if flights.contains_key(key) {
                return false;
            }
            flights.insert(key.to_owned(), Arc::default());
        }
        let fetched = self.fetch_entry(root, key, "prefetch", &self.prefetch.received);
        self.land(key, &fetched);
        matches!(fetched, Ok(true))
    }

    /// Ends the fetch of `key` in progress, telling whoever waits for it how.
    fn land(&self, key: &str, fetched: &Result<bool>) {
        let outcome = match fetched {
            Ok(true) => Outcome::Published,
            Ok(false) => Outcome::Absent,
            Err(_) => Outcome::Failed,
        };
        if let Some(flight) = self.flights.lock().unwrap().remove(key) {
            *flight.outcome.lock().unwrap() = Some(outcome);
            flight.done.notify_all();
        }
    }

    /// Notes that a task of this run used the entry for `key`, which the
    /// store holds.
    fn confirm(&self, key: &str) {
        self.confirmed.lock().unwrap().insert(key.to_owned());
        self.used(key);
    }

    /// Notes that the run resolved `key`, so prefetching can tell whether the
    /// run still follows the runs it fetches ahead for.
    pub(crate) fn resolved(&self, key: &str) {
        self.prefetch.resolved(key);
    }

    /// Whether the local entry for `key` was fetched ahead and no task has
    /// claimed it yet.
    pub(crate) fn prefetched(&self, key: &str) -> bool {
        self.prefetch.holds(key)
    }

    /// Claims the entry for `key` fetched ahead, for the task restoring it,
    /// returning whether it was.
    pub(crate) fn claim(&self, key: &str) -> bool {
        let claimed = self.prefetch.claim(key);
        if claimed {
            self.confirm(key);
        }
        claimed
    }

    /// Copies the entry for `key` into the local store at `root`, adding the
    /// bytes received to `received`.
    fn fetch_entry(
        &self,
        root: &Path,
        key: &str,
        task: &str,
        received: &AtomicU64,
    ) -> Result<bool> {
        let waiting = crate::profile::span(task, "remote_index_wait");
        let known = self.known();
        drop(waiting);
        if known.is_some_and(|known| !known.keys.contains_key(key)) {
            return Ok(false);
        }
        let listed = known
            .and_then(|known| known.apart.get(key))
            .cloned()
            .unwrap_or_default();
        let object = Object::Entry(key);
        let Some((manifest, pin, apart)) =
            self.fetch_pack(root, object, task, received, &listed, |parsed| {
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
        // The published manifest now keeps its blobs.
        drop(pin);
        self.kept_apart
            .lock()
            .unwrap()
            .insert(key.to_owned(), apart);
        Ok(true)
    }

    /// Whether the store holds the entry for `key`, as its index says, or as
    /// the store answers when the index could not be read.
    pub(crate) fn holds(&self, key: &str) -> Result<bool> {
        match self.known() {
            Some(known) => Ok(known.keys.contains_key(key)),
            None => self.store.has(Object::Entry(key)),
        }
    }

    /// Notes that the local cache held the entry for `key`.
    pub(crate) fn held(&self, key: &str) {
        self.held.lock().unwrap().insert(key.to_owned());
        self.used(key);
    }

    /// Notes that this run used the entry for `key`, if it has not yet.
    fn used(&self, key: &str) {
        let mut order = self.order.lock().unwrap();
        if order.1.insert(key.to_owned()) {
            order.0.push(key.to_owned());
        }
    }

    /// Queues the local entry for `key` for upload.
    pub(crate) fn upload(self: &Arc<Self>, root: &Path, key: &str) {
        self.used(key);
        self.enqueue(Upload {
            key: key.to_owned(),
            local: root.to_owned(),
            warm: None,
            path_sets: None,
            expectations: None,
            record_file: None,
        });
    }

    /// Queues the path sets recorded for `weak`, as the local cache keeps
    /// them now.
    pub(crate) fn upload_path_sets(self: &Arc<Self>, root: &Path, weak: &str, record: Vec<u8>) {
        self.enqueue(Upload {
            key: format!("{weak} path sets"),
            local: root.to_owned(),
            warm: None,
            path_sets: Some(PathSets {
                weak: weak.to_owned(),
                record,
            }),
            expectations: None,
            record_file: None,
        });
    }

    /// The path sets the store holds for `weak`, as their record, or `None`
    /// when it holds none for it.
    pub(crate) fn fetch_path_sets(
        &self,
        root: &Path,
        weak: &str,
        task: &str,
    ) -> Result<Option<Vec<u8>>> {
        let received = AtomicU64::new(0);
        let fetched =
            self.fetch_pack(root, Object::PathSets(weak), task, &received, &[], |_| {
                Ok(())
            })?;
        Ok(fetched.map(|(record, _, _)| record))
    }

    /// Queues a task's warm record for its branch, after the blobs it cites.
    pub(crate) fn upload_warm(
        self: &Arc<Self>,
        root: &Path,
        task: &str,
        identity: &str,
        branch: &str,
        record: Vec<u8>,
    ) {
        self.enqueue(Upload {
            key: format!("{task} warm state"),
            local: root.to_owned(),
            warm: Some(Warm {
                identity: identity.to_owned(),
                branch: branch.to_owned(),
                record,
            }),
            path_sets: None,
            expectations: None,
            record_file: None,
        });
    }

    /// Queues tasks' `runs`, ended at `now`, to be added to the expectations
    /// the store holds and saved for the first of `branches`.
    pub(crate) fn upload_expectations(
        self: &Arc<Self>,
        root: &Path,
        branches: Vec<String>,
        runs: BTreeMap<String, crate::expectations::Expectation>,
        now: u64,
    ) {
        self.enqueue(Upload {
            key: "task expectations".to_owned(),
            local: root.to_owned(),
            warm: None,
            path_sets: None,
            expectations: Some(Expectations {
                branches,
                runs,
                now,
            }),
            record_file: None,
        });
    }

    /// The record of tasks' expectations the store holds for `branch`, or
    /// `None` when it holds none.
    pub(crate) fn fetch_expectations(&self, root: &Path, branch: &str) -> Result<Option<Vec<u8>>> {
        let received = AtomicU64::new(0);
        let object = Object::Expectations(branch);
        let fetched = self.fetch_pack(root, object, "run", &received, &[], |_| Ok(()))?;
        Ok(fetched.map(|(record, _, _)| record))
    }

    /// The first of `branches` whose record of tasks' expectations can be
    /// read, with the branch it was saved for: one missing, or one that is
    /// not a record for its branch, gives way to the next.
    pub(crate) fn fetch_usable_expectations(
        &self,
        root: &Path,
        branches: &[String],
    ) -> Result<Option<(Vec<u8>, String)>> {
        for branch in branches {
            if let Some(record) = self.fetch_expectations(root, branch)?
                && crate::expectations::usable(&record, branch)
            {
                return Ok(Some((record, branch.clone())));
            }
        }
        Ok(None)
    }

    /// The task's warm record saved on `branch`, with every blob it cites
    /// fetched into the local store and verified, or `None` when there is
    /// none or `accept` rejects its key or the worktree root it was saved in.
    /// Both are checked before any blob is read, so a save that does not suit
    /// this checkout costs only its record.
    /// The pin keeps the blobs until the caller has stored the record.
    pub(crate) fn fetch_warm(
        &self,
        root: &Path,
        identity: &str,
        task: &str,
        branch: &str,
        accept: impl Fn(&[String], Option<&str>) -> bool,
    ) -> Result<Option<(Vec<u8>, crate::evict::Pin)>> {
        let object = Object::Warm { identity, branch };
        let received = AtomicU64::new(0);
        let fetched = self.fetch_pack(root, object, task, &received, &[], |record| {
            if record
                .warm_key()
                .is_some_and(|key| accept(&key, record.warm_worktree()))
            {
                Ok(())
            } else {
                Err(OtherWarmKey.into())
            }
        });
        match fetched {
            Err(error) if error.is::<OtherWarmKey>() => Ok(None),
            fetched => fetched.map(|fetched| fetched.map(|(record, pin, _)| (record, pin))),
        }
    }

    /// The record in the pack at `object`, once `check` accepts it and the
    /// blobs it cites that the local store lacks are kept there, with a pin on
    /// all of them, or `None` when there is no such object. Blobs `listed` as
    /// stored apart that the local store lacks are fetched beside the pack
    /// rather than after it. `task` names the task the fetch is for in profile
    /// timings, and the bytes received are added to `received`.
    fn fetch_pack(
        &self,
        root: &Path,
        object: Object,
        task: &str,
        received: &AtomicU64,
        listed: &[String],
        check: impl Fn(&crate::record::StoredRecord) -> Result<()>,
    ) -> Result<Option<Fetched>> {
        let early: Vec<String> = listed
            .iter()
            .filter(|hash| !root.join("blobs").join(hash.as_str()).is_file())
            .cloned()
            .collect();
        // Held until the pack's own pin covers them.
        let _early_pin = match early.is_empty() {
            true => None,
            false => Some(crate::evict::pin(root, &early.iter().cloned().collect())?),
        };
        let apart = AtomicU64::new(0);
        std::thread::scope(|scope| {
            let early_fetch = (!early.is_empty()).then(|| {
                // A blob the index wrongly lists is fetched after the pack.
                scope.spawn(|| self.fetch_blobs(root, object, &early, task, &apart).ok())
            });
            let early_fetch = Mutex::new(early_fetch);
            let wait_early = || {
                if let Some(fetching) = early_fetch.lock().unwrap().take() {
                    let _ = fetching.join();
                }
            };
            let Some(body) = self.store.get(object, task)? else {
                wait_early();
                return Ok(None);
            };
            let mut receiving = crate::profile::span(task, "remote_body");
            let mut transfer = pack::Transfer::default();
            let read = pack::read(root, body, check, &mut transfer, |missing| {
                wait_early();
                let missing: Vec<String> = missing
                    .into_iter()
                    .filter(|hash| !root.join("blobs").join(hash).is_file())
                    .collect();
                self.fetch_blobs(root, object, &missing, task, &apart)
            });
            wait_early();
            receiving.count("bytes", transfer.received);
            receiving.count("held_bytes", transfer.held);
            receiving.count("apart_bytes", apart.load(Ordering::Relaxed));
            receiving.count("skipped_bytes", transfer.skipped);
            received.fetch_add(
                transfer.received + apart.load(Ordering::Relaxed),
                Ordering::Relaxed,
            );
            match read {
                Err(error) if error.is::<MissingBlob>() => Ok(None),
                read => read.map(|(record, pin)| Some((record, pin, transfer.apart))),
            }
        })
    }

    /// Fetches `blobs`, stored apart, into the local store at `root`, a few
    /// at a time, adding the bytes received to `received`.
    fn fetch_blobs(
        &self,
        root: &Path,
        owner: Object,
        blobs: &[String],
        task: &str,
        received: &AtomicU64,
    ) -> Result<()> {
        let mut fetching = crate::profile::span(task, "remote_blobs");
        fetching.count("blobs", blobs.len() as u64);
        let queue = Mutex::new(blobs.iter());
        let fetch = || -> Result<()> {
            loop {
                let Some(hash) = queue.lock().unwrap().next() else {
                    return Ok(());
                };
                let _slot = self.blob_requests.take();
                let body = self
                    .store
                    .get_blob(hash, owner, task)?
                    .ok_or_else(|| MissingBlob(hash.clone()))?;
                let bytes = pack::read_blob(root, hash, body)?;
                received.fetch_add(bytes, Ordering::Relaxed);
            }
        };
        std::thread::scope(|scope| {
            let workers: Vec<_> = (0..blobs.len().min(BLOB_FETCHERS))
                .map(|_| scope.spawn(fetch))
                .collect();
            // Every worker is joined before an error is returned, so none
            // outlives the pin the caller holds.
            let results: Vec<Result<()>> = workers
                .into_iter()
                .map(|worker| worker.join().expect("blob fetch panicked"))
                .collect();
            results.into_iter().collect()
        })
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
        if let Some(sets) = &job.path_sets {
            // As with warm records, the newest save replaces the last one.
            return self.put_pack(&job.local, Object::PathSets(&sets.weak), &sets.record);
        }
        if let Some(shared) = &job.expectations {
            let Some(branch) = shared.branches.first() else {
                return Ok(());
            };
            // The runs join what the store holds now, so the tasks they leave
            // out keep theirs; a store that cannot say is not overwritten.
            let base = self.fetch_usable_expectations(&job.local, &shared.branches)?;
            let record = crate::expectations::merge(
                base.as_ref()
                    .map(|(record, from)| (record.as_slice(), from.as_str())),
                branch,
                &shared.runs,
                shared.now,
            );
            return self.put_pack(&job.local, Object::Expectations(branch), &record);
        }
        let object = Object::Entry(&job.key);
        let exists = self.store.has(object)?;
        // Blobs stored apart go up first, so the announcement can name them
        // and the entry never cites one the store lacks.
        let placed = if exists {
            None
        } else {
            let manifest = fs::read(job.local.join("entries").join(format!("{}.json", job.key)))?;
            let placements = self.place_all(&job.local, &manifest)?;
            Some((manifest, placements))
        };
        let apart: Vec<String> = match &placed {
            Some((_, placements)) => apart_of(placements),
            None => self
                .known()
                .and_then(|known| known.apart.get(&job.key).cloned())
                .unwrap_or_default(),
        };
        let announcement = self.store.announce(&job.key, &apart)?;
        if let Some((manifest, placements)) = placed {
            self.put_placed(&job.local, object, &manifest, &placements)?;
        }
        self.kept_apart
            .lock()
            .unwrap()
            .insert(job.key.clone(), apart);
        self.confirmed.lock().unwrap().insert(job.key.clone());
        self.announcements.lock().unwrap().extend(announcement);
        Ok(())
    }

    /// Uploads `record` with the blobs it cites from the local store at
    /// `root`: large blobs on their own, once the store lacks them, then a
    /// pack with the rest, so the pack never cites a blob not yet stored.
    fn put_pack(&self, root: &Path, object: Object, record: &[u8]) -> Result<()> {
        let placements = self.place_all(root, record)?;
        self.put_placed(root, object, record, &placements)
    }

    /// Where packs carry each blob `record` cites, uploading those stored
    /// apart that the store lacks.
    fn place_all(&self, root: &Path, record: &[u8]) -> Result<HashMap<String, pack::Placement>> {
        let parsed = crate::record::StoredRecord::read(record)?;
        let mut placements = HashMap::new();
        // A store server takes one bounded body per record, so once the record,
        // its header and its inline blobs fill that, the rest of this record's
        // blobs go apart. Raw sizes bound the compressed ones; each blob's
        // header entry is a hash and two numbers.
        let bounded = self.store.framing() == store::Framing::Upload;
        let mut inline = record.len() as u64 + 128 * parsed.blobs().len() as u64 + 64;
        for blob in parsed.blobs() {
            let mut placement = self.place(root, &blob)?;
            if bounded && placement == pack::Placement::Inline {
                let size = fs::metadata(root.join("blobs").join(&blob))?.len();
                if inline + size > UPLOAD_INLINE {
                    self.store_apart(root, &blob, None)?;
                    placement = pack::Placement::Apart;
                } else {
                    inline += size;
                }
            }
            placements.insert(blob, placement);
        }
        Ok(placements)
    }

    /// Uploads `record` as `object`, carrying its blobs as `placements` says.
    fn put_placed(
        &self,
        root: &Path,
        object: Object,
        record: &[u8],
        placements: &HashMap<String, pack::Placement>,
    ) -> Result<()> {
        let place = |blob: &str| {
            placements
                .get(blob)
                .copied()
                .ok_or_else(|| anyhow::anyhow!("blob {blob} was not placed"))
        };
        let mut pack = tempfile::NamedTempFile::new_in(root.join("tmp"))?;
        match self.store.framing() {
            store::Framing::Pack => pack::write(root, record, place, pack.as_file_mut())?,
            store::Framing::Upload => pack::write_upload(root, record, place, pack.as_file_mut())?,
        }
        if pack.as_file().metadata()?.len() > LARGEST_UPLOAD {
            bail!("too large to upload as one object");
        }
        match self.store.put(object, pack.reopen()?) {
            // The store lost blobs this process took it to hold: send them,
            // and commit once more.
            Err(error) if error.is::<store::MissingBlobs>() => {
                let missing = error.downcast::<store::MissingBlobs>().unwrap().0;
                for blob in &missing {
                    self.stored.lock().unwrap().remove(blob);
                    self.store_apart(root, blob, None)?;
                }
                self.store.put(object, pack.reopen()?)
            }
            put => put,
        }
    }

    /// Decides where packs carry the blob `hash`, uploading it on its own if
    /// it goes apart and the store lacks it. A blob goes apart when its
    /// compressed size, what it costs to transfer, reaches the threshold.
    fn place(&self, root: &Path, hash: &str) -> Result<pack::Placement> {
        let known = self.placements.lock().unwrap().get(hash).copied();
        let placement = match known {
            Some(placement) => placement,
            None => {
                let size = fs::metadata(root.join("blobs").join(hash))?.len();
                let compressed = if size < self.threshold {
                    None
                } else {
                    Some(pack::compress(root, hash)?).filter(|compressed| {
                        compressed
                            .as_file()
                            .metadata()
                            .is_ok_and(|metadata| metadata.len() >= self.threshold)
                    })
                };
                let placement = match compressed {
                    None => pack::Placement::Inline,
                    Some(compressed) => {
                        self.store_apart(root, hash, Some(compressed))?;
                        pack::Placement::Apart
                    }
                };
                self.placements
                    .lock()
                    .unwrap()
                    .insert(hash.to_owned(), placement);
                return Ok(placement);
            }
        };
        if placement == pack::Placement::Apart {
            self.store_apart(root, hash, None)?;
        }
        Ok(placement)
    }

    /// Uploads the blob `hash` on its own unless the store is known to hold
    /// it, or answers that it does.
    fn store_apart(
        &self,
        root: &Path,
        hash: &str,
        compressed: Option<tempfile::NamedTempFile>,
    ) -> Result<()> {
        if self.stored.lock().unwrap().contains(hash) {
            return Ok(());
        }
        if !self.store.has(Object::Blob(hash))? {
            let compressed = match compressed {
                Some(compressed) => compressed,
                None => pack::compress(root, hash)?,
            };
            self.store.put(Object::Blob(hash), compressed.reopen()?)?;
        }
        self.stored.lock().unwrap().insert(hash.to_owned());
        Ok(())
    }

    /// Waits for queued uploads, then adds this run's listing to the index,
    /// returning what failed.
    pub(crate) fn finish(&self) -> Vec<String> {
        // Prefetches in progress finish on their own; the run does not wait
        // for what it may not need.
        self.prefetch.stop();
        let (entries, claimed) = self.prefetch.summary();
        if entries > 0 {
            let mut summary = crate::profile::span("run", "remote_prefetch");
            summary.count("entries", entries as u64);
            summary.count("claimed", claimed as u64);
            summary.count("bytes", self.prefetch.received.load(Ordering::Relaxed));
        }
        if let Some(configuration) = &self.background {
            let pending = std::mem::take(&mut *self.pending.lock().unwrap());
            if !pending.is_empty() {
                let launched = background::launch(
                    configuration,
                    &pending,
                    self.held.lock().unwrap().iter().cloned().collect(),
                    self.confirmed.lock().unwrap().iter().cloned().collect(),
                    self.order.lock().unwrap().0.clone(),
                );
                match launched {
                    // Uploads whose files are gone could not be sent from
                    // here either.
                    Ok((log, skipped)) => {
                        qk_executor::status!(
                            "qk: remote uploads continuing in background; log: {}",
                            log.display()
                        );
                        return skipped;
                    }
                    Err(error) => {
                        qk_executor::status!(
                            "qk: could not detach remote uploads; waiting instead ({error:#})"
                        );
                        let mut failures = self.send_all(&pending);
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

    /// Sends `jobs` from this process, as many at a time as the run's own
    /// uploads, returning those that failed.
    fn send_all(&self, jobs: &[Upload]) -> Vec<String> {
        let queue = Mutex::new(jobs.iter());
        let failures = Mutex::new(Vec::new());
        std::thread::scope(|scope| {
            for _ in 0..UPLOADERS.min(jobs.len()) {
                scope.spawn(|| {
                    loop {
                        let Some(job) = queue.lock().unwrap().next() else {
                            return;
                        };
                        if let Err(error) = self.send(job) {
                            failures
                                .lock()
                                .unwrap()
                                .push(format!("{}: {error:#}", job.key));
                        }
                    }
                });
            }
        });
        failures.into_inner().unwrap()
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
        let members = std::mem::take(&mut self.order.lock().unwrap().0)
            .into_iter()
            .filter(|key| confirmed.contains(key))
            .collect();
        let kept = std::mem::take(&mut *self.kept_apart.lock().unwrap());
        let apart = confirmed
            .iter()
            .filter_map(|key| {
                let hashes = kept
                    .get(key)
                    .or_else(|| known.and_then(|known| known.apart.get(key)))?;
                Some((key.clone(), hashes.clone()))
            })
            .collect();
        self.store
            .list(confirmed, apart, members, known, announcements)
    }
}

/// A fetched pack's record, the pin on the blobs it cites, and the blobs it
/// stores apart.
type Fetched = (Vec<u8>, crate::evict::Pin, Vec<String>);

/// The blobs `placements` stores apart, in order.
fn apart_of(placements: &HashMap<String, pack::Placement>) -> Vec<String> {
    let mut apart: Vec<String> = placements
        .iter()
        .filter(|(_, placement)| **placement == pack::Placement::Apart)
        .map(|(blob, _)| blob.clone())
        .collect();
    apart.sort();
    apart
}

/// How `flight` ended, or `None` if `cancelled` first.
fn wait(flight: &Flight, cancelled: &std::sync::atomic::AtomicBool) -> Option<Outcome> {
    let mut outcome = flight.outcome.lock().unwrap();
    loop {
        if let Some(outcome) = *outcome {
            return Some(outcome);
        }
        if cancelled.load(Ordering::SeqCst) {
            return None;
        }
        outcome = flight
            .done
            .wait_timeout(outcome, Duration::from_millis(50))
            .unwrap()
            .0;
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

#[cfg(all(test, unix))]
mod agent_tests {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::os::unix::thread::JoinHandleExt;
    use std::sync::mpsc::channel;
    use std::time::Duration;

    extern "C" fn interrupt(_: libc::c_int) {}

    #[test]
    fn a_request_outlasts_a_signal_while_it_waits_for_the_answer() {
        // SAFETY: the handler does nothing, and no other test uses SIGUSR1.
        // Without SA_RESTART, a read the signal interrupts fails with EINTR on
        // every Unix, as a socket read with a timeout does on Linux whatever
        // the handler.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = interrupt as extern "C" fn(libc::c_int) as libc::sighandler_t;
            libc::sigemptyset(&mut action.sa_mask);
            assert_eq!(
                libc::sigaction(libc::SIGUSR1, &action, std::ptr::null_mut()),
                0
            );
        }
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let (asked, request) = channel();
        let (release, released) = channel::<()>();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(&stream);
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap() > 2 {
                line.clear();
            }
            asked.send(()).unwrap();
            released.recv().unwrap();
            (&stream)
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .unwrap();
        });
        let client = std::thread::spawn(move || {
            super::agent()
                .get(url)
                .call()
                .map(|response| response.status().as_u16())
                .map_err(|error| error.to_string())
        });
        // The request is sent; signal the client while it waits for the answer.
        request.recv().unwrap();
        for _ in 0..20 {
            // SAFETY: the thread is not joined yet, so its handle is valid.
            unsafe { libc::pthread_kill(client.as_pthread_t(), libc::SIGUSR1) };
            std::thread::sleep(Duration::from_millis(5));
        }
        release.send(()).unwrap();
        assert_eq!(client.join().unwrap(), Ok(200));
        server.join().unwrap();
    }

    /// A stalled request times out on time whatever signals arrive, over
    /// HTTP and over HTTPS, where rustls restarts interrupted reads itself.
    #[test]
    fn signals_do_not_keep_a_stalled_request_waiting_past_its_timeout() {
        for scheme in ["http", "https"] {
            let (elapsed, result) = stalled(scheme);
            assert!(result.unwrap_err().contains("timeout"), "{scheme}");
            assert!(elapsed < Duration::from_secs(2), "{scheme}: {elapsed:?}");
        }
    }

    /// A request to a server that takes the connection and never answers,
    /// with its client signalled every 20 ms: how long it took, and how it
    /// ended.
    fn stalled(scheme: &str) -> (Duration, Result<(), String>) {
        // SAFETY: as above; installing the same handler again is harmless.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = interrupt as extern "C" fn(libc::c_int) as libc::sighandler_t;
            libc::sigemptyset(&mut action.sa_mask);
            assert_eq!(
                libc::sigaction(libc::SIGUSR1, &action, std::ptr::null_mut()),
                0
            );
        }
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("{scheme}://{}/", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || listener.accept().unwrap());
        let client = std::thread::spawn(move || {
            let config = ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_millis(300)))
                .build();
            let agent = ureq::Agent::with_parts(
                config,
                super::connector(),
                ureq::unversioned::resolver::DefaultResolver::default(),
            );
            let started = std::time::Instant::now();
            let result = agent.get(url).call().map(|_| ());
            (started.elapsed(), result.map_err(|error| error.to_string()))
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while !client.is_finished() && std::time::Instant::now() < deadline {
            // SAFETY: the thread is not joined yet, so its handle is valid.
            unsafe { libc::pthread_kill(client.as_pthread_t(), libc::SIGUSR1) };
            std::thread::sleep(Duration::from_millis(20));
        }
        let ended = client.join().unwrap();
        drop(server.join());
        ended
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

#[cfg(test)]
mod flight_tests {
    use super::store::{self, Body, Object, Store};
    use super::*;

    /// A store in memory whose reads take a while and are counted.
    #[derive(Default)]
    struct Memory {
        objects: Mutex<HashMap<String, Vec<u8>>>,
        reads: Arc<AtomicUsize>,
        /// Whether it takes upload bodies, as a store server does.
        uploads: bool,
    }

    fn name(object: Object) -> String {
        match object {
            Object::Entry(key) => format!("entries/{key}"),
            Object::Warm { identity, branch } => format!("warm/{identity}/{branch}"),
            Object::Blob(hash) => format!("blobs/{hash}"),
            Object::PathSets(weak) => format!("pathsets/{weak}"),
            Object::Expectations(branch) => format!("expectations/{branch}"),
        }
    }

    impl Store for Memory {
        fn location(&self) -> String {
            "memory".into()
        }

        fn framing(&self) -> store::Framing {
            if self.uploads {
                store::Framing::Upload
            } else {
                store::Framing::Pack
            }
        }

        fn sync_index(&self, _: &mut index::Known) -> Result<()> {
            Ok(())
        }

        fn get(&self, object: Object, _: &str) -> Result<Option<Body>> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(200));
            let objects = self.objects.lock().unwrap();
            Ok(objects
                .get(&name(object))
                .map(|bytes| Box::new(std::io::Cursor::new(bytes.clone())) as Body))
        }

        fn has(&self, object: Object) -> Result<bool> {
            Ok(self.objects.lock().unwrap().contains_key(&name(object)))
        }

        fn put(&self, object: Object, mut file: fs::File) -> Result<()> {
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            self.objects.lock().unwrap().insert(name(object), bytes);
            Ok(())
        }

        fn announce(&self, _: &str, _: &[String]) -> Result<Option<String>> {
            Ok(None)
        }

        fn list(
            &self,
            _: BTreeSet<String>,
            _: HashMap<String, Vec<String>>,
            _: Vec<String>,
            _: Option<&index::Known>,
            _: BTreeSet<String>,
        ) -> Result<()> {
            Ok(())
        }
    }

    fn cache() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        for name in ["entries", "blobs", "tmp"] {
            fs::create_dir(root.path().join(name)).unwrap();
        }
        root
    }

    #[test]
    fn an_upload_to_a_store_server_carries_a_bounded_amount_inline() {
        let source = cache();
        let mut blobs = Vec::new();
        for index in 0..3u8 {
            let path = source.path().join("blobs").join(format!("{index}"));
            // Sparse, and the same bytes each time; the names stand in for
            // hashes, which placement never checks.
            fs::File::create(&path)
                .unwrap()
                .set_len(UPLOAD_INLINE / 2 - 4096)
                .unwrap();
            let name = format!("{index}").repeat(64);
            fs::rename(&path, source.path().join("blobs").join(&name)).unwrap();
            blobs.push(name);
        }
        let record = |note: &str| {
            serde_json::to_vec(&serde_json::json!({
                "note": note,
                "artifacts": blobs
                    .iter()
                    .map(|blob| (blob.clone(), serde_json::json!({"blob": blob})))
                    .collect::<serde_json::Map<_, _>>()
            }))
            .unwrap()
        };
        let remote = || {
            let memory = Memory {
                uploads: true,
                ..Memory::default()
            };
            Remote::new(Box::new(memory), Mode::ReadWrite, None, u64::MAX)
        };
        let small = remote();
        let placements = small.place_all(source.path(), &record("")).unwrap();
        // Two fit under the bound; the third goes apart.
        assert_eq!(placements[&blobs[0]], pack::Placement::Inline);
        assert_eq!(placements[&blobs[1]], pack::Placement::Inline);
        assert_eq!(placements[&blobs[2]], pack::Placement::Apart);
        assert!(small.store.has(Object::Blob(&blobs[2])).unwrap());
        // The record counts against the bound too.
        let large = remote();
        let placements = large
            .place_all(source.path(), &record(&"x".repeat(16 << 10)))
            .unwrap();
        assert_eq!(placements[&blobs[0]], pack::Placement::Inline);
        assert_eq!(placements[&blobs[1]], pack::Placement::Apart);
    }

    #[test]
    fn expectations_saved_for_a_branch_start_from_the_default_branch() {
        let root = cache();
        let remote = Arc::new(Remote::new(
            Box::new(Memory::default()),
            Mode::ReadWrite,
            None,
            u64::MAX,
        ));
        let run = |millis| crate::expectations::Expectation {
            millis,
            cores: 4,
            memory: None,
        };
        let runs = |list: &[(&str, u64)]| {
            list.iter()
                .map(|(id, millis)| (id.to_string(), run(*millis)))
                .collect::<BTreeMap<_, _>>()
        };
        let branches = |list: &[&str]| list.iter().map(|branch| branch.to_string()).collect();
        remote.upload_expectations(
            root.path(),
            branches(&["main"]),
            runs(&[("app:a", 10), ("app:b", 20)]),
            1_000,
        );
        assert!(remote.finish().is_empty());
        assert!(
            remote
                .fetch_expectations(root.path(), "feature")
                .unwrap()
                .is_none()
        );
        remote.upload_expectations(
            root.path(),
            branches(&["feature", "main"]),
            runs(&[("app:a", 30)]),
            2_000,
        );
        assert!(remote.finish().is_empty());
        let saved = remote
            .fetch_expectations(root.path(), "feature")
            .unwrap()
            .unwrap();
        let expected = crate::expectations::read(&saved, "feature");
        // Of a's runs, 30 and 10, the median is the later.
        assert_eq!(expected["app:a"].millis, 30);
        assert_eq!(expected["app:b"].millis, 20);
        // A record that cannot be read gives way to the default branch's,
        // when fetching and when adding to it.
        remote
            .put_pack(root.path(), Object::Expectations("broken"), b"{}")
            .unwrap();
        let (fallback, from) = remote
            .fetch_usable_expectations(root.path(), &branches(&["broken", "main"]))
            .unwrap()
            .unwrap();
        assert_eq!(from, "main");
        assert_eq!(
            crate::expectations::read(&fallback, &from)["app:b"].millis,
            20
        );
        remote.upload_expectations(
            root.path(),
            branches(&["broken", "main"]),
            runs(&[("app:a", 40)]),
            3_000,
        );
        assert!(remote.finish().is_empty());
        let repaired = remote
            .fetch_expectations(root.path(), "broken")
            .unwrap()
            .unwrap();
        assert_eq!(
            crate::expectations::read(&repaired, "broken")["app:b"].millis,
            20
        );
        // The default branch's record is left as it was.
        let main = remote
            .fetch_expectations(root.path(), "main")
            .unwrap()
            .unwrap();
        assert_eq!(crate::expectations::read(&main, "main")["app:a"].millis, 10);
        // A store that may only be read is not written.
        let read = Arc::new(Remote::new(
            Box::new(Memory::default()),
            Mode::Read,
            None,
            u64::MAX,
        ));
        read.upload_expectations(root.path(), branches(&["main"]), runs(&[("app:a", 1)]), 1);
        assert!(read.finish().is_empty());
        assert!(
            read.fetch_expectations(root.path(), "main")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn concurrent_fetches_of_one_key_download_it_once() {
        let source = cache();
        let log = source.path().join("blobs/log");
        fs::write(&log, "log").unwrap();
        let hash = crate::hash::digest_file(&log).unwrap();
        fs::rename(&log, source.path().join("blobs").join(&hash)).unwrap();
        let key = "a".repeat(64);
        let record = serde_json::to_vec(&serde_json::json!({
            "version": 1, "key": key, "log": hash, "artifacts": {}
        }))
        .unwrap();
        let mut pack = Vec::new();
        pack::write(
            source.path(),
            &record,
            |_| Ok(pack::Placement::Inline),
            &mut pack,
        )
        .unwrap();
        let memory = Memory::default();
        let reads = memory.reads.clone();
        memory
            .objects
            .lock()
            .unwrap()
            .insert(format!("entries/{key}"), pack);
        let remote = Arc::new(Remote::new(Box::new(memory), Mode::Read, None, u64::MAX));
        let destination = cache();
        let cancelled = std::sync::atomic::AtomicBool::new(false);
        let fetched: Vec<bool> = std::thread::scope(|scope| {
            let fetches: Vec<_> = (0..3)
                .map(|_| {
                    scope.spawn(|| {
                        remote
                            .fetch(destination.path(), &key, "app:build", &cancelled)
                            .unwrap()
                    })
                })
                .collect();
            fetches
                .into_iter()
                .map(|fetch| fetch.join().unwrap())
                .collect()
        });
        assert_eq!(fetched, [true, true, true]);
        assert!(
            destination
                .path()
                .join("entries")
                .join(format!("{key}.json"))
                .is_file()
        );
        assert_eq!(reads.load(Ordering::SeqCst), 1);
    }
}
