//! The remote store behind a store server, configured by nx.json's
//! `qk:storeServer` key. The server answers each fetch with the blobs the client
//! lacks, keeps the index itself and commits entries atomically; its protocol
//! is in `tools/store-server/PROTOCOL.md`.

use std::collections::BTreeSet;
use std::fs::File;
use std::io;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use super::bloom::Bloom;
use super::index;
use super::store::{Body, Framing, MissingBlobs, Object, Store, warm_names};

/// The prefix the local copy of a store server's index names its cursor by.
const CURSOR: &str = "cursor:";

pub(super) struct Server {
    url: String,
    pub(super) token: String,
    agent: ureq::Agent,
    /// The session naming the blobs this machine holds, once the server has
    /// it: `None` when there is none.
    session: OnceLock<Option<String>>,
    /// The session being made in the background.
    sessioning: Mutex<Option<JoinHandle<Option<String>>>>,
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
        // What of the wait the server spent, as it says: the rest is the
        // network and what is in front of it.
        if let Some(timing) = response
            .headers()
            .get("server-timing")
            .and_then(|value| value.to_str().ok())
        {
            waiting.note("server", timing);
        }
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
        if !matches!(object, Object::Blob(_))
            && let Some(session) = self.session()
        {
            path.push_str(&format!("?session={session}"));
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
            Object::Blob(_) | Object::PathSets(_) | Object::Expectations(_) => String::new(),
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
}
