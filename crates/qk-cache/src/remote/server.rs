//! The remote store behind a store server, configured by nx.json's
//! `qk:storeServer` key. The server answers each fetch with the blobs the client
//! lacks, keeps the index itself and commits entries atomically; its protocol
//! is in `tools/store-server/PROTOCOL.md`.

use std::collections::BTreeSet;
use std::fs::File;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use super::bloom::Bloom;
use super::index;
use super::store::{Body, Framing, Held, MissingBlobs, Object, Store, warm_names};

/// The prefix the local copy of a store server's index names its cursor by.
const CURSOR: &str = "cursor:";
/// Blobs asked about in one `POST blobs/missing`, as many as the protocol
/// allows.
const MISSING_QUERY: usize = 4096;

pub(super) struct Server {
    url: String,
    pub(super) token: String,
    agent: ureq::Agent,
    /// The session naming the blobs this machine holds, once the server has
    /// it: `None` when there is none.
    session: OnceLock<Option<String>>,
    /// The session being made in the background.
    sessioning: Mutex<Option<JoinHandle<Option<String>>>>,
    /// Whether the server answered `POST blobs/fetch` with `404`, as one that
    /// sends blobs only one each.
    fetches_one_each: AtomicBool,
    /// Whether the server answered `POST pathsets/fetch` with `404`, as one
    /// that sends path sets only one weak key each.
    path_sets_one_each: AtomicBool,
}

/// The store server `config`, nx.json's `qk:storeServer` object, names, with its
/// token from `QK_STORE_SERVER_TOKEN`.
pub(super) fn configure(
    config: &Value,
    variable: impl Fn(&str) -> Option<String>,
) -> Result<Server> {
    let url = variable("QK_STORE_SERVER_URL")
        .or_else(|| config.get("url").and_then(Value::as_str).map(str::to_owned))
        .context("qk:storeServer.url is required")?;
    let url = url.trim_end_matches('/').to_owned();
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        bail!("invalid qk:storeServer.url {url:?}");
    }
    let token = variable("QK_STORE_SERVER_TOKEN").context("no token: set QK_STORE_SERVER_TOKEN")?;
    Ok(Server {
        url,
        token,
        agent: super::agent(),
        session: OnceLock::new(),
        sessioning: Mutex::new(None),
        fetches_one_each: AtomicBool::new(false),
        path_sets_one_each: AtomicBool::new(false),
    })
}

impl Server {
    fn path(&self, object: Object) -> String {
        match object {
            Object::Entry(key) => format!("entries/{key}"),
            Object::Warm { identity, branch } => {
                let (identity, branch) = warm_names(identity, branch);
                format!("warm/{identity}/{branch}")
            }
            Object::Adoptable { identity, key } => {
                format!("adoptable/{}/{key}", super::store::warm_identity(identity))
            }
            Object::Blob(hash) => format!("blobs/{hash}"),
            Object::PathSets(weak) => format!("pathsets/{weak}"),
            Object::Expectations(branch) => {
                format!("expectations/{}", super::store::expectations_name(branch))
            }
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}/v4/{path}", self.url)
    }

    fn authorization(&self) -> String {
        format!("Bearer {}", self.token)
    }

    /// The session, waiting for it to be made. It is made beside the index,
    /// which a fetch waits for too.
    fn session(&self) -> Option<&str> {
        self.session
            .get_or_init(|| {
                let sessioning = self.sessioning.lock().unwrap().take()?;
                sessioning.join().ok().flatten()
            })
            .as_deref()
    }

    /// A GET of `path` that answers with a body, or `None` on 404.
    fn read(&self, path: &str, task: &str) -> Result<Option<Body>> {
        let mut waiting = crate::profile::span(task, "remote_response");
        let response = self
            .agent
            .get(self.url(path))
            .header("Authorization", self.authorization())
            .call()?;
        timed(&mut waiting, &response);
        drop(waiting);
        match response.status().as_u16() {
            200 => Ok(Some(Box::new(io::BufReader::new(
                response.into_body().into_reader(),
            )))),
            404 => Ok(None),
            _ => Err(failure(&format!("GET {path}"), response)),
        }
    }
}

impl Store for Server {
    fn location(&self) -> String {
        self.url.clone()
    }

    fn framing(&self) -> Framing {
        Framing::Upload
    }

    /// Tells the server which blobs the local cache holds, in the background,
    /// so fetches made once it knows leave them out.
    fn begin(&self, root: &Path) {
        let root = root.to_owned();
        let agent = self.agent.clone();
        let url = self.url("sessions");
        let authorization = self.authorization();
        let sessioning = std::thread::spawn(move || {
            // With no blobs, there is nothing to leave out.
            let bloom = Bloom::for_blobs(&root)?;
            match open_session(&agent, &url, &authorization, &bloom) {
                Ok(session) => Some(session),
                Err(error) => {
                    qk_executor::status!(
                        "qk: could not tell the store server which blobs this machine holds, so fetches send them again ({error:#})"
                    );
                    None
                }
            }
        });
        *self.sessioning.lock().unwrap() = Some(sessioning);
    }

    fn sync_index(&self, known: &mut index::Known) -> Result<()> {
        let since = known
            .listings
            .iter()
            .find_map(|name| name.strip_prefix(CURSOR))
            .map(str::to_owned);
        let path = match &since {
            Some(cursor) => format!("index?since={cursor}"),
            None => "index".to_owned(),
        };
        let response = self
            .agent
            .get(self.url(&path))
            .header("Authorization", self.authorization())
            .call()?;
        if response.status() != 200 {
            return Err(failure("GET index", response));
        }
        let cursor = response
            .headers()
            .get("Qk-Index-Cursor")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
            .context("the store server's index has no cursor")?;
        let body = super::read_bounded(
            response.into_body().into_reader(),
            super::LARGEST_INDEX,
            None,
        )?;
        // The server lists every run it remembers each time.
        known.runs.clear();
        known.add(&String::from_utf8_lossy(&body));
        known.listings = BTreeSet::from([format!("{CURSOR}{cursor}")]);
        Ok(())
    }

    fn get(&self, object: Object, task: &str) -> Result<Option<Body>> {
        let mut path = self.path(object);
        // Path sets and expectations are records citing no blob, so the
        // session has nothing to leave out of them, and they are not held up
        // until it is made.
        let cites_blobs = !matches!(
            object,
            Object::Blob(_) | Object::PathSets(_) | Object::Expectations(_)
        );
        if cites_blobs {
            let waiting = crate::profile::span(task, "remote_session_wait");
            let session = self.session();
            drop(waiting);
            if let Some(session) = session {
                path.push_str(&format!("?session={session}"));
            }
        }
        self.read(&path, task)
    }

    fn get_blob(&self, hash: &str, owner: Object, task: &str) -> Result<Option<Body>> {
        let owner = match owner {
            Object::Entry(key) => format!("entry={key}"),
            Object::Warm { identity, branch } => {
                let (identity, branch) = warm_names(identity, branch);
                format!("warm={identity}/{branch}")
            }
            Object::Adoptable { .. }
            | Object::Blob(_)
            | Object::PathSets(_)
            | Object::Expectations(_) => String::new(),
        };
        self.read(&format!("blobs/{hash}?{owner}"), task)
    }

    fn has(&self, object: Object) -> Result<bool> {
        let path = self.path(object);
        let response = self
            .agent
            .head(self.url(&path))
            .header("Authorization", self.authorization())
            .call()?;
        match response.status().as_u16() {
            200 => Ok(true),
            404 => Ok(false),
            _ => Err(failure(&format!("HEAD {path}"), response)),
        }
    }

    fn put(&self, object: Object, file: File) -> Result<()> {
        let path = self.path(object);
        let content = match object {
            Object::Blob(_) => "application/zstd",
            _ => "application/x-qk-upload",
        };
        let response = self
            .agent
            .put(self.url(&path))
            .header("Authorization", self.authorization())
            .header("Content-Type", content)
            .send(file)?;
        match response.status().as_u16() {
            200..=299 => Ok(()),
            // Another run committed the entry first.
            409 if matches!(object, Object::Entry(_)) => Ok(()),
            // A server that keeps no warm records by result.
            404 if matches!(object, Object::Adoptable { .. }) => Ok(()),
            422 => {
                let answer: Value = serde_json::from_str(&response.into_body().read_to_string()?)?;
                let blobs = answer
                    .get("blobs")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .filter(|hash| crate::record::valid_hash(hash))
                    .map(str::to_owned)
                    .collect();
                Err(MissingBlobs(blobs).into())
            }
            _ => Err(failure(&format!("PUT {path}"), response)),
        }
    }

    fn adopt(&self, identity: &str, branch: &str, key: &str) -> Result<()> {
        let path = format!("{}/adopt", self.path(Object::Warm { identity, branch }));
        let response = self
            .agent
            .post(self.url(&path))
            .header("Authorization", self.authorization())
            .header("Content-Type", "application/json")
            .send(serde_json::json!({ "key": key }).to_string())?;
        match response.status().as_u16() {
            200..=299 => Ok(()),
            // No run saved a record for this result, or the server keeps none
            // by result.
            404 => Ok(()),
            _ => Err(failure(&format!("POST {path}"), response)),
        }
    }

    fn get_blobs(&self, blobs: &[String], task: &str) -> Result<Option<Body>> {
        if self.fetches_one_each.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let waiting = crate::profile::span(task, "remote_response");
        let response = self
            .agent
            .post(self.url("blobs/fetch"))
            .header("Authorization", self.authorization())
            .header("Content-Type", "application/json")
            .send(serde_json::json!({ "blobs": blobs }).to_string())?;
        drop(waiting);
        match response.status().as_u16() {
            200 => Ok(Some(Box::new(io::BufReader::new(
                response.into_body().into_reader(),
            )))),
            404 => {
                self.fetches_one_each.store(true, Ordering::Relaxed);
                Ok(None)
            }
            _ => Err(failure("POST blobs/fetch", response)),
        }
    }

    fn get_path_sets(&self, weak: &[String], task: &str) -> Result<Option<Body>> {
        if self.path_sets_one_each.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let mut waiting = crate::profile::span(task, "remote_response");
        let response = self
            .agent
            .post(self.url("pathsets/fetch"))
            .header("Authorization", self.authorization())
            .header("Content-Type", "application/json")
            .send(serde_json::json!({ "weak": weak }).to_string())?;
        timed(&mut waiting, &response);
        drop(waiting);
        match response.status().as_u16() {
            200 => Ok(Some(Box::new(io::BufReader::new(
                response.into_body().into_reader(),
            )))),
            404 => {
                self.path_sets_one_each.store(true, Ordering::Relaxed);
                Ok(None)
            }
            _ => Err(failure("POST pathsets/fetch", response)),
        }
    }

    fn blobs_held(&self, blobs: &[String]) -> Result<Option<Held>> {
        let mut held = Held::default();
        for asked in blobs.chunks(MISSING_QUERY) {
            let response = self
                .agent
                .post(self.url("blobs/missing"))
                .header("Authorization", self.authorization())
                .header("Content-Type", "application/json")
                .send(serde_json::json!({ "blobs": asked }).to_string())?;
            if response.status().as_u16() != 200 {
                return Err(failure("POST blobs/missing", response));
            }
            let answer: Value = serde_json::from_str(&response.into_body().read_to_string()?)?;
            // A server that takes bundles always names the loose ones.
            let Some(loose) = answer.get("loose").and_then(Value::as_array) else {
                return Ok(None);
            };
            let names = |values: &Vec<Value>| -> Vec<String> {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|hash| crate::record::valid_hash(hash))
                    .map(str::to_owned)
                    .collect()
            };
            held.loose.extend(names(loose));
            let missing = answer
                .get("missing")
                .and_then(Value::as_array)
                .context("no missing blobs in the store server's answer")?;
            held.missing.extend(names(missing));
        }
        Ok(Some(held))
    }

    fn put_bundle(&self, id: &str, file: File) -> Result<bool> {
        let path = format!("bundles/{id}");
        let response = self
            .agent
            .put(self.url(&path))
            .header("Authorization", self.authorization())
            .header("Content-Type", "application/x-qk-upload")
            .send(file)?;
        match response.status().as_u16() {
            200..=299 => Ok(true),
            404 => Ok(false),
            _ => Err(failure(&format!("PUT {path}"), response)),
        }
    }

    fn announce(&self, _key: &str, _apart: &[String]) -> Result<Option<String>> {
        // The server lists an entry when it commits it.
        Ok(None)
    }

    fn list(
        &self,
        keys: BTreeSet<String>,
        // The server sends a client the blobs it lacks with the entry.
        _apart: std::collections::HashMap<String, Vec<String>>,
        members: Vec<String>,
        _known: Option<&index::Known>,
        _announcements: BTreeSet<String>,
    ) -> Result<()> {
        let now = index::now();
        let mut text = index::format(keys.iter().map(|key| (key.as_str(), now)));
        let run = index::Run {
            time: now,
            members: members
                .iter()
                .map(|key| index::member(key).to_owned())
                .collect(),
        };
        text.push_str(&index::format_runs(&std::collections::BTreeMap::from([(
            index::name(now),
            run,
        )])));
        let response = self
            .agent
            .post(self.url("runs"))
            .header("Authorization", self.authorization())
            .header("Content-Type", "text/plain")
            .send(text.as_bytes())?;
        match response.status().as_u16() {
            200..=299 => Ok(()),
            _ => Err(failure("POST runs", response)),
        }
    }
}

/// Notes on `waiting` what of the wait the server spent, as `response` says:
/// the rest is the network and what is in front of it.
fn timed(waiting: &mut crate::profile::Span<'_>, response: &ureq::http::Response<ureq::Body>) {
    if let Some(timing) = response
        .headers()
        .get("server-timing")
        .and_then(|value| value.to_str().ok())
    {
        waiting.note("server", timing);
    }
}

/// The reason a response failed, from its status and JSON error.
fn failure(what: &str, response: ureq::http::Response<ureq::Body>) -> anyhow::Error {
    let status = response.status().as_u16();
    let body = response.into_body().read_to_string().unwrap_or_default();
    let message = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|error| {
            error
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or(body);
    match status {
        401 => anyhow::anyhow!("the store server rejected the token ({what})"),
        403 => anyhow::anyhow!("the store server token is read-only ({what})"),
        status => anyhow::anyhow!("{what} returned {status}: {message}"),
    }
}

/// Opens a session naming the blobs in `bloom`, returning its id.
fn open_session(
    agent: &ureq::Agent,
    url: &str,
    authorization: &str,
    bloom: &Bloom,
) -> Result<String> {
    let response = agent
        .post(url)
        .header("Authorization", authorization)
        .header("Content-Type", "application/x-qk-bloom")
        .send(&bloom.to_bytes()[..])?;
    if response.status() != 201 {
        return Err(failure("POST sessions", response));
    }
    let answer: Value = serde_json::from_str(&response.into_body().read_to_string()?)
        .context("POST sessions answered with something other than JSON")?;
    answer
        .get("session")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .context("POST sessions answered without a session")
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;

    use super::*;

    /// The URL of a server that reads one request whole and answers it with
    /// `answer`.
    fn answer_once(answer: String) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v4/sessions", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(&stream);
            let mut length = 0;
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap() > 2 {
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap();
                }
                line.clear();
            }
            reader.read_exact(&mut vec![0; length]).unwrap();
            (&stream).write_all(answer.as_bytes()).unwrap();
        });
        url
    }

    fn bloom() -> Bloom {
        let mut bloom = Bloom::new(1);
        bloom.insert(&"a".repeat(64));
        bloom
    }

    #[test]
    fn a_session_is_the_id_the_server_gives() {
        let body = r#"{"session":"0123456789abcdef0123456789abcdef","expires":1}"#;
        let answer = format!(
            "HTTP/1.1 201 Created\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let url = answer_once(answer);
        let session = open_session(&super::super::agent(), &url, "Bearer t", &bloom()).unwrap();
        assert_eq!(session, "0123456789abcdef0123456789abcdef");
    }

    #[test]
    fn a_refused_session_says_why() {
        let body = r#"{"error":"busy","message":"the store could not answer; try again"}"#;
        let answer = format!(
            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let url = answer_once(answer);
        let error = open_session(&super::super::agent(), &url, "Bearer t", &bloom()).unwrap_err();
        assert_eq!(
            format!("{error:#}"),
            "POST sessions returned 503: the store could not answer; try again"
        );
    }

    /// A server at the URL `answer_once` gives.
    fn server_at(url: &str) -> Server {
        let base = url.trim_end_matches("/v4/sessions");
        configure(&serde_json::json!({ "url": base }), |name| {
            (name == "QK_STORE_SERVER_TOKEN").then(|| "t".to_owned())
        })
        .unwrap()
    }

    #[test]
    fn a_server_without_many_path_sets_at_once_is_asked_one_each_from_then_on() {
        let body = r#"{"error":"not_found","message":"no such request"}"#;
        let answer = format!(
            "HTTP/1.1 404 Not Found\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let server = server_at(&answer_once(answer));
        let weak = ["a".repeat(64)];
        assert!(server.get_path_sets(&weak, "run").unwrap().is_none());
        // Not asked again: the one answer it gives was given.
        assert!(server.get_path_sets(&weak, "run").unwrap().is_none());
    }

    #[test]
    fn many_path_sets_at_once_are_the_answer_the_server_gives() {
        let mut body = b"qks1".to_vec();
        body.extend_from_slice(&1u32.to_le_bytes());
        // Bytes that are text too, as `answer_once` sends text.
        body.extend_from_slice(&[0x11; 32]);
        body.extend_from_slice(&0u64.to_le_bytes());
        let mut answer =
            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
        answer.extend_from_slice(&body);
        let server = server_at(&answer_once(String::from_utf8(answer).unwrap()));
        let weak = ["11".repeat(32)];
        let mut received = Vec::new();
        server
            .get_path_sets(&weak, "run")
            .unwrap()
            .unwrap()
            .read_to_end(&mut received)
            .unwrap();
        assert_eq!(received, body);
    }

    #[test]
    fn a_refused_request_for_many_path_sets_says_why() {
        let body = r#"{"error":"busy","message":"the store could not answer; try again"}"#;
        let answer = format!(
            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let server = server_at(&answer_once(answer));
        let Err(error) = server.get_path_sets(&["a".repeat(64)], "run") else {
            panic!("a refused request was taken as an answer");
        };
        assert_eq!(
            format!("{error:#}"),
            "POST pathsets/fetch returned 503: the store could not answer; try again"
        );
    }
}
