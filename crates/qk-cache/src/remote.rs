//! A remote store on S3-compatible storage, configured by nx.json's `s3` key as
//! `@nx/s3-cache` reads it.
//!
//! Objects live under `<cacheKeyPrefix>qk/v2/`, so a bucket shared with Nx never
//! mixes the two: `entries/<key>` for results and `warm/<task>/<branch>` for
//! warm records. Each object is a pack holding a manifest or record with every
//! blob it cites, so that one request reads or writes it whole; the store's
//! latency is per request, and an entry can cite thousands of blobs. A local
//! miss fetches the pack and keeps the blobs missing locally, verifying each
//! against its hash; the restore then runs locally. Uploads run in the
//! background after a task is saved. An [`index`] of the entries the store
//! holds spares the lookups it would answer with a miss.

mod index;

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rusty_s3::actions::ListObjectsV2;
use rusty_s3::{Bucket, Credentials, S3Action, UrlStyle};
use serde_json::Value;

/// How long a signed request stays valid; requests are made at once.
const SIGNED_FOR: Duration = Duration::from_secs(15 * 60);
/// Uploads in flight at once.
const UPLOADERS: usize = 8;
/// The most a single upload may hold, as S3 allows.
const LARGEST_UPLOAD: u64 = 5 << 30;
/// Listings fetched at once.
const LISTING_READERS: usize = 16;
/// The most a pack's manifest or record may hold, so a corrupt length is not
/// taken for one to read into memory.
const LARGEST_RECORD: u64 = 1 << 30;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    ReadWrite,
    Read,
}

pub struct Remote {
    bucket: Bucket,
    credentials: Credentials,
    prefix: String,
    agent: ureq::Agent,
    pub mode: Mode,
    uploads: Mutex<Option<Uploads>>,
    /// What the store's index says it holds, once read: `None` when it could
    /// not be read, and every lookup then goes to the store.
    known: OnceLock<Option<index::Known>>,
    /// The index being read in the background.
    reading: Mutex<Option<JoinHandle<Option<index::Known>>>>,
    /// Entries this run found in the store or put there, for its listing.
    confirmed: Mutex<BTreeSet<String>>,
    /// Entries this run found in the local cache, which its listing names
    /// too when the index already does.
    held: Mutex<BTreeSet<String>>,
}

struct Uploads {
    sender: Sender<Upload>,
    workers: Vec<JoinHandle<()>>,
    failures: Arc<Mutex<Vec<String>>>,
}

struct Upload {
    /// The entry's key, or the warm record's task, for reporting failures.
    key: String,
    local: PathBuf,
    /// A warm record to write, with its object name, instead of the entry.
    warm: Option<(String, Vec<u8>)>,
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
    if setting("encryptionKey").is_some() {
        bail!("s3.encryptionKey is not supported");
    }
    let bucket_name = setting("bucket").context("s3.bucket is required")?;
    let region = setting("region").context("s3.region is required")?;
    let endpoint =
        setting("endpoint").unwrap_or_else(|| format!("https://s3.{region}.amazonaws.com"));
    let style = if config.get("forcePathStyle").and_then(Value::as_bool) == Some(true) {
        UrlStyle::Path
    } else {
        UrlStyle::VirtualHost
    };
    let bucket = Bucket::new(
        endpoint
            .parse()
            .with_context(|| format!("invalid s3.endpoint {endpoint:?}"))?,
        style,
        bucket_name,
        region,
    )
    .map_err(|error| anyhow::anyhow!("invalid s3 configuration: {error:?}"))?;
    let key = setting("accessKeyId").or_else(|| variable("AWS_ACCESS_KEY_ID"));
    let secret = setting("secretAccessKey").or_else(|| variable("AWS_SECRET_ACCESS_KEY"));
    let (Some(key), Some(secret)) = (key, secret) else {
        bail!("no credentials: set AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY");
    };
    let credentials = match variable("AWS_SESSION_TOKEN") {
        Some(token) => Credentials::new_with_token(key, secret, token),
        None => Credentials::new(key, secret),
    };
    let agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_global(Some(Duration::from_secs(10 * 60)))
        .build()
        .into();
    Ok(Some(Remote {
        bucket,
        credentials,
        prefix: format!("{}qk/v2/", setting("cacheKeyPrefix").unwrap_or_default()),
        agent,
        mode,
        uploads: Mutex::new(None),
        known: OnceLock::new(),
        reading: Mutex::new(None),
        confirmed: Mutex::default(),
        held: Mutex::default(),
    }))
}

impl Remote {
    fn object(&self, kind: &str, name: &str) -> String {
        format!("{}{kind}/{name}", self.prefix)
    }

    /// Starts reading the store's index in the background, keeping a copy in
    /// the local cache at `root` so that later runs fetch only what is new.
    pub(crate) fn read_index(self: &Arc<Self>, root: &Path) {
        let store = self
            .bucket
            .object_url(&self.prefix)
            .map_or_else(|_| self.prefix.clone(), |url| url.to_string());
        let path = root
            .join("remote")
            .join(&blake3::hash(store.as_bytes()).to_hex()[..32]);
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

    /// The local copy at `path` brought up to date with the listings the store
    /// holds now.
    fn sync_index(&self, path: &Path) -> Result<index::Known> {
        let mut known = index::Known::load(path);
        let names = self.list(&self.object("index", ""))?;
        let new: Vec<&String> = names
            .iter()
            .filter(|name| !known.listings.contains(*name))
            .collect();
        let fetched: Vec<Result<Option<Vec<u8>>>> = std::thread::scope(|scope| {
            new.chunks(new.len().div_ceil(LISTING_READERS).max(1))
                .map(|names| {
                    scope.spawn(move || {
                        names
                            .iter()
                            .map(|name| self.get(&self.object("index", name)))
                            .collect::<Vec<_>>()
                    })
                })
                .collect::<Vec<_>>()
                .into_iter()
                .flat_map(|reader| reader.join().unwrap())
                .collect()
        });
        for listing in fetched {
            // Compaction may replace a listing after LIST. This snapshot is
            // then incomplete and cannot safely rule out any entry.
            let Some(listing) = listing? else {
                bail!("remote index changed while it was read");
            };
            known.add(&String::from_utf8_lossy(&listing));
        }
        known.listings = names.into_iter().collect();
        known.expire(index::now());
        if let Err(error) = known.save(path) {
            qk_executor::status!("qk: could not save the remote cache index ({error:#})");
        }
        Ok(known)
    }

    /// The names of the objects under `prefix`, without it.
    fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let mut names = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut action = self.bucket.list_objects_v2(Some(&self.credentials));
            action.with_prefix(prefix);
            if let Some(token) = &token {
                action.with_continuation_token(token.as_str());
            }
            let url = action.sign(SIGNED_FOR);
            let response = self.agent.get(url.as_str()).call()?;
            let status = response.status().as_u16();
            let body = response.into_body().read_to_string()?;
            if status != 200 {
                bail!("listing {prefix} returned {status}");
            }
            let parsed = ListObjectsV2::parse_response(&body)
                .map_err(|error| anyhow::anyhow!("invalid listing of {prefix}: {error:?}"))?;
            names.extend(
                parsed
                    .contents
                    .into_iter()
                    .filter_map(|object| object.key.strip_prefix(prefix).map(str::to_owned)),
            );
            match parsed.next_continuation_token {
                Some(next) => token = Some(next),
                None => return Ok(names),
            }
        }
    }

    /// The object's body, or `None` when it is missing.
    fn get(&self, object: &str) -> Result<Option<Vec<u8>>> {
        let url = self
            .bucket
            .get_object(Some(&self.credentials), object)
            .sign(SIGNED_FOR);
        let response = self.agent.get(url.as_str()).call()?;
        let status = response.status().as_u16();
        let mut body = Vec::new();
        response.into_body().into_reader().read_to_end(&mut body)?;
        match status {
            200 => Ok(Some(body)),
            404 => Ok(None),
            status => bail!("GET {object} returned {status}"),
        }
    }

    fn delete(&self, object: &str) -> Result<()> {
        let url = self
            .bucket
            .delete_object(Some(&self.credentials), object)
            .sign(SIGNED_FOR);
        let response = self.agent.delete(url.as_str()).call()?;
        let status = response.status().as_u16();
        io::copy(&mut response.into_body().into_reader(), &mut io::sink())?;
        match status {
            200..=299 | 404 => Ok(()),
            status => bail!("DELETE {object} returned {status}"),
        }
    }

    fn exists(&self, object: &str) -> Result<bool> {
        let url = self
            .bucket
            .head_object(Some(&self.credentials), object)
            .sign(SIGNED_FOR);
        let response = self.agent.head(url.as_str()).call()?;
        match response.status().as_u16() {
            200 => Ok(true),
            404 => Ok(false),
            status => bail!("HEAD {object} returned {status}"),
        }
    }

    /// Uploads with a known length: S3 refuses chunked uploads to signed URLs.
    fn put(&self, object: &str, body: impl ureq::AsSendBody) -> Result<()> {
        let url = self
            .bucket
            .put_object(Some(&self.credentials), object)
            .sign(SIGNED_FOR);
        let response = self.agent.put(url.as_str()).send(body)?;
        let status = response.status().as_u16();
        io::copy(&mut response.into_body().into_reader(), &mut io::sink())?;
        match status {
            200..=299 => Ok(()),
            status => bail!("PUT {object} returned {status}"),
        }
    }

    /// Copies the entry for `key` into the local store at `root`, returning
    /// whether the remote had it. Every blob the local store lacks is verified
    /// before it is kept.
    pub(crate) fn fetch(&self, root: &Path, key: &str) -> Result<bool> {
        if self
            .known()
            .is_some_and(|known| !known.keys.contains_key(key))
        {
            return Ok(false);
        }
        let Some(manifest) = self.fetch_pack(root, &self.object("entries", key), |parsed| {
            if parsed.get("key").and_then(Value::as_str) != Some(key) {
                bail!("remote manifest is for another key");
            }
            Ok(())
        })?
        else {
            return Ok(false);
        };
        let mut file = tempfile::NamedTempFile::new_in(root.join("tmp"))?;
        file.write_all(&manifest)?;
        file.as_file().sync_all()?;
        file.persist(root.join("entries").join(format!("{key}.json")))?;
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
            warm: Some((self.warm_object(task, branch), record)),
        });
    }

    fn warm_object(&self, task: &str, branch: &str) -> String {
        let hash = |text: &str| blake3::hash(text.as_bytes()).to_hex()[..32].to_owned();
        let identity = format!(
            "{task}\0{}\0{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        );
        self.object(
            "warm",
            &format!("{}/{}.json", hash(&identity), hash(branch)),
        )
    }

    /// The task's warm record saved on `branch`, with every blob it cites
    /// fetched into the local store and verified, or `None` when there is
    /// none.
    pub(crate) fn fetch_warm(
        &self,
        root: &Path,
        task: &str,
        branch: &str,
    ) -> Result<Option<Vec<u8>>> {
        self.fetch_pack(root, &self.warm_object(task, branch), |_| Ok(()))
    }

    /// The record in the pack at `object`, once `check` accepts it and the
    /// blobs it cites that the local store lacks are kept there, or `None`
    /// when there is no such object.
    fn fetch_pack(
        &self,
        root: &Path,
        object: &str,
        check: impl Fn(&Value) -> Result<()>,
    ) -> Result<Option<Vec<u8>>> {
        let url = self
            .bucket
            .get_object(Some(&self.credentials), object)
            .sign(SIGNED_FOR);
        let response = self.agent.get(url.as_str()).call()?;
        let status = response.status().as_u16();
        let mut body = io::BufReader::new(response.into_body().into_reader());
        if status != 200 {
            io::copy(&mut body, &mut io::sink())?;
            match status {
                404 => return Ok(None),
                status => bail!("GET {object} returned {status}"),
            }
        }
        let length = pack_length(&mut body)?;
        if length > LARGEST_RECORD {
            bail!("remote record in {object} is too large");
        }
        let mut record = Vec::new();
        if (&mut body).take(length).read_to_end(&mut record)? as u64 != length {
            bail!("remote pack {object} ends early");
        }
        let parsed: Value = serde_json::from_slice(&record).context("invalid remote record")?;
        check(&parsed)?;
        for blob in crate::evict::manifest_blobs(&parsed) {
            if !crate::store::valid_hash(&blob) {
                bail!("invalid blob name in remote record");
            }
            let length = pack_length(&mut body)?;
            let mut part = (&mut body).take(length);
            let target = root.join("blobs").join(&blob);
            let copied = if target.is_file() {
                io::copy(&mut part, &mut io::sink())?
            } else {
                let mut file = tempfile::NamedTempFile::new_in(root.join("tmp"))?;
                let copied = io::copy(&mut part, file.as_file_mut())?;
                file.as_file().sync_all()?;
                if copied == length {
                    if crate::hash::digest_file(file.path())? != blob {
                        bail!("remote blob {blob} does not match its hash");
                    }
                    file.persist(&target)?;
                }
                copied
            };
            if copied != length {
                bail!("remote pack {object} ends early");
            }
        }
        if body.read(&mut [0])? != 0 {
            bail!("remote pack {object} holds more than its record cites");
        }
        Ok(Some(record))
    }

    fn enqueue(self: &Arc<Self>, upload: Upload) {
        if self.mode != Mode::ReadWrite {
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
        if let Some((object, record)) = &job.warm {
            // The newest save for the branch replaces the last one.
            return self.put_pack(&job.local, object, record);
        }
        let object = self.object("entries", &job.key);
        let exists = self.exists(&object)?;
        // Announce the key before publishing its entry. If the run stops or
        // the final listing fails, readers can still discover the entry. An
        // announcement whose upload fails merely causes a normal GET miss.
        let now = index::now();
        let listing = index::format(std::iter::once((job.key.as_str(), now)));
        self.put(&self.object("index", &index::name(now)), listing.as_bytes())?;
        if !exists {
            let manifest = fs::read(job.local.join("entries").join(format!("{}.json", job.key)))?;
            self.put_pack(&job.local, &object, &manifest)?;
        }
        self.confirmed.lock().unwrap().insert(job.key.clone());
        Ok(())
    }

    /// Uploads `record` with the blobs it cites from the local store at
    /// `root`, as one pack.
    fn put_pack(&self, root: &Path, object: &str, record: &[u8]) -> Result<()> {
        let parsed: Value = serde_json::from_slice(record)?;
        let mut pack = tempfile::NamedTempFile::new_in(root.join("tmp"))?;
        let mut writer = io::BufWriter::new(pack.as_file_mut());
        writer.write_all(&(record.len() as u64).to_le_bytes())?;
        writer.write_all(record)?;
        for blob in crate::evict::manifest_blobs(&parsed) {
            let mut file = File::open(root.join("blobs").join(&blob))?;
            let length = file.metadata()?.len();
            writer.write_all(&length.to_le_bytes())?;
            if io::copy(&mut (&mut file).take(length), &mut writer)? != length {
                bail!("blob {blob} changed while it was uploaded");
            }
        }
        writer.flush()?;
        drop(writer);
        let file = pack.reopen()?;
        if file.metadata()?.len() > LARGEST_UPLOAD {
            bail!("too large to upload as one object");
        }
        self.put(object, file)
    }

    /// Waits for queued uploads, then adds this run's listing to the index,
    /// returning what failed.
    pub(crate) fn finish(&self) -> Vec<String> {
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

    /// Lists the entries this run found in the store or put there. With many
    /// listings already, merges them into this one and deletes them.
    fn add_listing(&self) -> Result<()> {
        let mut confirmed = std::mem::take(&mut *self.confirmed.lock().unwrap());
        // A run that only used the local cache neither waits for the index nor
        // writes to it.
        if confirmed.is_empty() {
            return Ok(());
        }
        let now = index::now();
        let known = self.known();
        if let Some(known) = known {
            let held = std::mem::take(&mut *self.held.lock().unwrap());
            confirmed.extend(held.into_iter().filter(|key| known.keys.contains_key(key)));
        }
        let mut listing = index::Known::default();
        let merged = known.filter(|known| known.listings.len() >= index::MERGE_AFTER);
        if let Some(known) = merged {
            listing.keys = known.keys.clone();
        }
        listing
            .keys
            .extend(confirmed.into_iter().map(|key| (key, now)));
        listing.expire(now);
        let text = index::format(listing.keys.iter().map(|(key, time)| (key.as_str(), *time)));
        self.put(&self.object("index", &index::name(now)), text.as_bytes())?;
        for name in merged.iter().flat_map(|known| &known.listings) {
            self.delete(&self.object("index", name))?;
        }
        Ok(())
    }
}

/// The length that starts each part of a pack.
fn pack_length(reader: &mut impl Read) -> Result<u64> {
    let mut bytes = [0; 8];
    reader
        .read_exact(&mut bytes)
        .context("remote pack ends early")?;
    Ok(u64::from_le_bytes(bytes))
}
