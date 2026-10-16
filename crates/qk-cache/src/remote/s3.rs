//! The remote store on S3-compatible storage, configured by nx.json's `s3` key
//! as `@nx/s3-cache` reads it.

use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::io;
use std::sync::atomic::AtomicUsize;

use anyhow::{Context, Result, bail};
use rusty_s3::actions::{DeleteObjectsResponse, ListObjectsV2, ObjectIdentifier};
use rusty_s3::{Bucket, Credentials, S3Action, UrlStyle};
use serde_json::Value;

use super::index;
use super::store::{Body, Object, Store};

/// How long a signed request stays valid; requests are made at once.
const SIGNED_FOR: std::time::Duration = std::time::Duration::from_secs(15 * 60);
/// Listings fetched at once.
const LISTING_READERS: usize = 16;
/// Limits on index payloads, which are optional hints rather than task results.
const LARGEST_LISTING: usize = 8 << 20;
const LARGEST_LIST_RESPONSE: usize = 1 << 20;
/// The layout objects live under, after `cacheKeyPrefix`.
pub(super) const LAYOUT: &str = "qk/v4/";

pub(super) struct S3 {
    bucket: Bucket,
    pub(super) credentials: Credentials,
    prefix: String,
    agent: ureq::Agent,
}

/// The store `config`, nx.json's `s3` object, names, with credentials from it
/// or from `variable`.
pub(super) fn configure(config: &Value, variable: impl Fn(&str) -> Option<String>) -> Result<S3> {
    let setting = |name: &str| config.get(name).and_then(Value::as_str).map(str::to_owned);
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
    Ok(S3 {
        bucket,
        credentials,
        prefix: format!("{}{LAYOUT}", setting("cacheKeyPrefix").unwrap_or_default()),
        agent: super::agent(),
    })
}

impl S3 {
    fn object(&self, kind: &str, name: &str) -> String {
        format!("{}{kind}/{name}", self.prefix)
    }

    fn name(&self, object: Object) -> String {
        match object {
            Object::Entry(key) => self.object("entries", key),
            Object::Warm { identity, branch } => {
                let (identity, branch) = super::store::warm_names(identity, branch);
                self.object("warm", &format!("{identity}/{branch}.json"))
            }
            Object::Blob(hash) => self.object("blobs", hash),
        }
    }

    /// Names from a single complete LIST response. Pagination has no snapshot
    /// semantics: a replacement can sort before the continuation position while
    /// compaction removes an unseen old listing. Such an index cannot rule out
    /// entries, so use the direct-lookup fallback instead.
    fn names(&self, prefix: &str) -> Result<Vec<String>> {
        let mut action = self.bucket.list_objects_v2(Some(&self.credentials));
        action.with_prefix(prefix);
        let url = action.sign(SIGNED_FOR);
        let response = self.agent.get(url.as_str()).call()?;
        let status = response.status().as_u16();
        if status != 200 {
            bail!("listing {prefix} returned {status}");
        }
        let body = super::read_bounded(
            response.into_body().into_reader(),
            LARGEST_LIST_RESPONSE,
            None,
        )?;
        let body = String::from_utf8(body).context("invalid index listing response")?;
        let parsed = ListObjectsV2::parse_response(&body)
            .map_err(|error| anyhow::anyhow!("invalid listing of {prefix}: {error:?}"))?;
        if parsed.next_continuation_token.is_some() {
            bail!("paginated remote index cannot provide a complete snapshot");
        }
        Ok(parsed
            .contents
            .into_iter()
            .filter_map(|object| object.key.strip_prefix(prefix).map(str::to_owned))
            .collect())
    }

    /// A bounded listing body, or `None` when concurrent compaction removed it.
    fn listing(&self, object: &str, budget: &AtomicUsize) -> Result<Option<Vec<u8>>> {
        let url = self
            .bucket
            .get_object(Some(&self.credentials), object)
            .sign(SIGNED_FOR);
        let response = self.agent.get(url.as_str()).call()?;
        match response.status().as_u16() {
            200 => Ok(Some(super::read_bounded(
                response.into_body().into_reader(),
                LARGEST_LISTING,
                Some(budget),
            )?)),
            404 => Ok(None),
            status => bail!("GET {object} returned {status}"),
        }
    }

    /// Delete listings in batches, keeping cleanup latency per batch rather
    /// than per uploaded entry. S3 accepts at most 1,000 keys per request.
    fn delete_listings(&self, names: BTreeSet<String>) -> Result<()> {
        let objects: Vec<_> = names
            .into_iter()
            .map(|name| ObjectIdentifier::new(self.object("index", &name)))
            .collect();
        for objects in objects.chunks(1_000) {
            let mut action = self
                .bucket
                .delete_objects(Some(&self.credentials), objects.iter());
            action.set_quiet(true);
            let url = action.sign(SIGNED_FOR);
            let (body, content_md5) = action.body_with_md5();
            let response = self
                .agent
                .post(url.as_str())
                .header("Content-MD5", content_md5)
                .send(body)?;
            let status = response.status().as_u16();
            let body = response.into_body().read_to_string()?;
            if !(200..=299).contains(&status) {
                bail!("deleting index listings returned {status}");
            }
            let response = DeleteObjectsResponse::parse(&body)?;
            if !response.errors.is_empty() {
                bail!("deleting index listings failed: {:?}", response.errors);
            }
        }
        Ok(())
    }

    /// Uploads with a known length: S3 refuses chunked uploads to signed URLs.
    fn put_object(&self, object: &str, body: impl ureq::AsSendBody) -> Result<()> {
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
}

impl Store for S3 {
    fn location(&self) -> String {
        self.bucket
            .object_url(&self.prefix)
            .map_or_else(|_| self.prefix.clone(), |url| url.to_string())
    }

    fn sync_index(&self, known: &mut index::Known) -> Result<()> {
        let names = self.names(&self.object("index", ""))?;
        let new: Vec<&String> = names
            .iter()
            .filter(|name| !known.listings.contains(*name))
            .collect();
        let budget = AtomicUsize::new(0);
        let fetched: Vec<Result<Option<Vec<u8>>>> = std::thread::scope(|scope| {
            let budget = &budget;
            new.chunks(new.len().div_ceil(LISTING_READERS).max(1))
                .map(|names| {
                    scope.spawn(move || {
                        names
                            .iter()
                            .map(|name| self.listing(&self.object("index", name), budget))
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
        Ok(())
    }

    fn get(&self, object: Object, task: &str) -> Result<Option<Body>> {
        let object = self.name(object);
        let url = self
            .bucket
            .get_object(Some(&self.credentials), &object)
            .sign(SIGNED_FOR);
        // Until the store's response headers arrive: the round trip, and a
        // connection when none is open.
        let waiting = crate::profile::span(task, "remote_response");
        let response = self.agent.get(url.as_str()).call()?;
        drop(waiting);
        let status = response.status().as_u16();
        let mut body = io::BufReader::new(response.into_body().into_reader());
        if status != 200 {
            io::copy(&mut body, &mut io::sink())?;
            match status {
                404 => return Ok(None),
                status => bail!("GET {object} returned {status}"),
            }
        }
        Ok(Some(Box::new(body)))
    }

    fn has(&self, object: Object) -> Result<bool> {
        let object = self.name(object);
        let url = self
            .bucket
            .head_object(Some(&self.credentials), &object)
            .sign(SIGNED_FOR);
        let response = self.agent.head(url.as_str()).call()?;
        match response.status().as_u16() {
            200 => Ok(true),
            404 => Ok(false),
            status => bail!("HEAD {object} returned {status}"),
        }
    }

    fn put(&self, object: Object, file: File) -> Result<()> {
        self.put_object(&self.name(object), file)
    }

    // Announce the key before publishing its entry. If the run stops or the
    // final listing fails, readers can still discover the entry. An
    // announcement whose upload fails merely causes a normal GET miss.
    fn announce(&self, key: &str, apart: &[String]) -> Result<Option<String>> {
        let now = index::now();
        let mut listing = index::format(std::iter::once((key, now)));
        listing.push_str(&index::format_apart(&HashMap::from([(
            key.to_owned(),
            apart.to_vec(),
        )])));
        let announcement = index::name(now);
        self.put_object(&self.object("index", &announcement), listing.as_bytes())?;
        Ok(Some(announcement))
    }

    /// With many listings already, merges them into this one, keeping the
    /// history of recent runs, and deletes them.
    fn list(
        &self,
        keys: BTreeSet<String>,
        apart: HashMap<String, Vec<String>>,
        members: Vec<String>,
        known: Option<&index::Known>,
        announcements: BTreeSet<String>,
    ) -> Result<()> {
        let now = index::now();
        let name = index::name(now);
        let mut listing = index::Known::default();
        let merged = known.filter(|known| known.listings.len() >= index::MERGE_AFTER);
        if let Some(known) = merged {
            listing.keys = known.keys.clone();
            listing.runs = known.runs.clone();
            listing.apart = known.apart.clone();
        }
        listing.keys.extend(keys.into_iter().map(|key| (key, now)));
        listing.apart.extend(apart);
        listing.runs.insert(
            name.clone(),
            index::Run {
                time: now,
                members: members
                    .iter()
                    .map(|key| index::member(key).to_owned())
                    .collect(),
            },
        );
        listing.expire(now);
        let mut text = index::format(listing.keys.iter().map(|(key, time)| (key.as_str(), *time)));
        text.push_str(&index::format_runs(&listing.runs));
        text.push_str(&index::format_apart(&listing.apart));
        self.put_object(&self.object("index", &name), text.as_bytes())?;
        // The final listing now covers every confirmed announcement. Leave
        // announcements in place on any earlier failure, including uploads
        // whose outcome was uncertain, so entries remain discoverable.
        let mut obsolete = announcements;
        obsolete.extend(
            merged
                .iter()
                .flat_map(|known| known.listings.iter().cloned()),
        );
        self.delete_listings(obsolete)
    }
}
