//! A remote store on S3-compatible storage, configured by nx.json's `s3` key as
//! `@nx/s3-cache` reads it.
//!
//! It mirrors the local layout under `<cacheKeyPrefix>qk/v1/`: `entries/<key>.json`
//! manifests and `blobs/<hash>` contents, so a bucket shared with Nx never
//! mixes the two. A local miss fetches the manifest, then only the blobs missing
//! locally, verifying each against its hash; the restore then runs locally.
//! Uploads run in the background after a task is saved, blobs first and the
//! manifest last, so a reader never finds a manifest without its blobs.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rusty_s3::{Bucket, Credentials, S3Action, UrlStyle};
use serde_json::Value;

/// How long a signed request stays valid; requests are made at once.
const SIGNED_FOR: Duration = Duration::from_secs(15 * 60);
/// Uploads in flight at once.
const UPLOADERS: usize = 8;

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
        prefix: format!("{}qk/v1/", setting("cacheKeyPrefix").unwrap_or_default()),
        agent,
        mode,
        uploads: Mutex::new(None),
    }))
}

impl Remote {
    fn object(&self, kind: &str, name: &str) -> String {
        format!("{}{kind}/{name}", self.prefix)
    }

    /// The object's body streamed into `sink`, or `false` when it is missing.
    fn get(&self, object: &str, sink: &mut dyn Write) -> Result<bool> {
        let url = self
            .bucket
            .get_object(Some(&self.credentials), object)
            .sign(SIGNED_FOR);
        let response = self.agent.get(url.as_str()).call()?;
        match response.status().as_u16() {
            200 => {
                io::copy(&mut response.into_body().into_reader(), sink)?;
                Ok(true)
            }
            404 => Ok(false),
            status => bail!("GET {object} returned {status}"),
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
        match response.status().as_u16() {
            200..=299 => Ok(()),
            status => bail!("PUT {object} returned {status}"),
        }
    }

    /// Copies the entry for `key` into the local store at `root`, returning
    /// whether the remote had it. Blobs the local store already holds are not
    /// fetched again; every fetched blob is verified before it is kept.
    pub(crate) fn fetch(&self, root: &Path, key: &str) -> Result<bool> {
        let mut manifest = Vec::new();
        if !self.get(
            &self.object("entries", &format!("{key}.json")),
            &mut manifest,
        )? {
            return Ok(false);
        }
        let parsed: Value = serde_json::from_slice(&manifest).context("invalid remote manifest")?;
        if parsed.get("key").and_then(Value::as_str) != Some(key) {
            bail!("remote manifest is for another key");
        }
        for blob in crate::evict::manifest_blobs(&parsed) {
            self.fetch_blob(root, &blob)?;
        }
        let mut file = tempfile::NamedTempFile::new_in(root.join("tmp"))?;
        file.write_all(&manifest)?;
        file.as_file().sync_all()?;
        file.persist(root.join("entries").join(format!("{key}.json")))?;
        Ok(true)
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
        let mut record = Vec::new();
        if !self.get(&self.warm_object(task, branch), &mut record)? {
            return Ok(None);
        }
        let parsed: Value =
            serde_json::from_slice(&record).context("invalid remote warm record")?;
        for blob in crate::evict::manifest_blobs(&parsed) {
            self.fetch_blob(root, &blob)?;
        }
        Ok(Some(record))
    }

    fn fetch_blob(&self, root: &Path, blob: &str) -> Result<()> {
        if !crate::store::valid_hash(blob) {
            bail!("invalid blob name in remote record");
        }
        let target = root.join("blobs").join(blob);
        if target.is_file() {
            return Ok(());
        }
        let mut file = tempfile::NamedTempFile::new_in(root.join("tmp"))?;
        if !self.get(&self.object("blobs", blob), file.as_file_mut())? {
            bail!("remote record is missing blob {blob}");
        }
        file.as_file().sync_all()?;
        if crate::hash::digest_file(file.path())? != blob {
            bail!("remote blob {blob} does not match its hash");
        }
        file.persist(&target)?;
        Ok(())
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
            let parsed: Value = serde_json::from_slice(record)?;
            for blob in crate::evict::manifest_blobs(&parsed) {
                let remote = self.object("blobs", &blob);
                if !self.exists(&remote)? {
                    self.put(&remote, File::open(job.local.join("blobs").join(&blob))?)?;
                }
            }
            // The newest save for the branch replaces the last one.
            return self.put(object, record.as_slice());
        }
        let manifest_path = job.local.join("entries").join(format!("{}.json", job.key));
        let manifest = fs::read(&manifest_path)?;
        let parsed: Value = serde_json::from_slice(&manifest)?;
        let object = self.object("entries", &format!("{}.json", job.key));
        if self.exists(&object)? {
            return Ok(());
        }
        for blob in crate::evict::manifest_blobs(&parsed) {
            let remote = self.object("blobs", &blob);
            if !self.exists(&remote)? {
                self.put(&remote, File::open(job.local.join("blobs").join(&blob))?)?;
            }
        }
        self.put(&object, manifest.as_slice())
    }

    /// Waits for queued uploads, returning the ones that failed.
    pub(crate) fn finish(&self) -> Vec<String> {
        let Some(uploads) = self.uploads.lock().unwrap().take() else {
            return Vec::new();
        };
        drop(uploads.sender);
        for worker in uploads.workers {
            let _ = worker.join();
        }
        std::mem::take(&mut *uploads.failures.lock().unwrap())
    }
}
