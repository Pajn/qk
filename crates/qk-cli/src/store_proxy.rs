//! `qk store proxy`: a store server in front of another, for machines on one
//! network. Blobs never change once stored, so the proxy keeps those it passes
//! on its own disk and serves them from there. It takes uploads of blobs,
//! entries and warm state at the network's speed: it keeps them, queues them
//! on its disk and answers at once, sending them upstream in the background.
//! Every other request goes to the upstream server as it came, so reads of
//! records that change and the index stay the upstream's. Runners point
//! `QK_STORE_SERVER_URL` at the proxy and keep their tokens, which the
//! upstream checks.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// The response headers the protocol gives meaning to, passed on as they came.
const PASSED_HEADERS: [&str; 4] = [
    "content-type",
    "retry-after",
    "qk-index-cursor",
    "server-timing",
];

/// The largest request body the proxy takes, and the longest request or
/// header line, past which a request is refused rather than buffered.
const MAX_BODY: usize = 1 << 30;
const MAX_LINE: u64 = 64 << 10;
const MAX_HEADERS: usize = 100;
/// How long a connection may wait for a client to send or take bytes.
const IDLE: Duration = Duration::from_secs(60);
/// After how long the proxy reads the index whole again, as a listing of what
/// changed cannot say which keys the upstream dropped.
const INDEX_REREAD: Duration = Duration::from_secs(60 * 60);
/// How long reading the index may take, after which a whole listing is
/// asked of the upstream as it came.
const INDEX_TIMEOUT: Duration = Duration::from_secs(60);
/// The largest index the proxy reads into memory.
const LARGEST_INDEX: u64 = 256 << 20;
/// The largest bundle a store server takes, as the protocol bounds an upload
/// body.
const LARGEST_BUNDLE: usize = 100 << 20;
/// Queued uploads sent upstream at once.
const UPLOADERS: usize = 32;
/// Blobs asked about in one `POST blobs/missing`, of the 4,096 the protocol
/// allows.
const MISSING_BATCH: usize = 1024;
/// The longest pause before sending a queued upload again; the pause doubles
/// from a second up to it.
const RETRY_PAUSE: Duration = Duration::from_secs(60);
/// Times the upstream may answer that it lacks a record's blobs, each sent
/// again, before the record is given up.
const MISSING_ROUNDS: u32 = 3;
/// Blobs one `POST blobs/fetch` may ask for, as the protocol allows.
const FETCH_QUERY: usize = 4096;
/// How many bytes of frames one answer to `POST blobs/fetch` carries, as a
/// store server's does.
const FETCH_BUDGET: u64 = 64 << 20;

pub struct Options {
    pub upstream: String,
    pub directory: PathBuf,
    pub listen: String,
    pub max_size: Option<u64>,
}

struct Proxy {
    agent: ureq::Agent,
    upstream: String,
    blobs: PathBuf,
    scratch: PathBuf,
    /// Uploads answered for and not yet sent upstream, each a job file named
    /// by its place in the queue and, for a record, its body beside it.
    queue: PathBuf,
    /// The records the proxy serves without asking the upstream, named by a
    /// hash of their paths: each entry it committed upstream, which never
    /// changes once committed; and the newest upload of each warm record, as
    /// warm state is never current, only recent, so a save the proxy holds
    /// serves as well as a newer one elsewhere.
    records: PathBuf,
    /// The upstream's index as last read, which whole listings are answered
    /// from once brought up to date; and held while one is, so that listings
    /// asked for meanwhile share it.
    index: Mutex<Option<IndexCopy>>,
    /// Held while the copy is brought up to date; it holds when the last
    /// attempt that failed began, so that listings asked for before then
    /// share its failure rather than each trying again.
    catching_up: Mutex<Option<Instant>>,
    next_job: AtomicU64,
    jobs: Mutex<VecDeque<u64>>,
    job_ready: Condvar,
    /// Blobs queued for upload, which eviction leaves alone, with the ids of
    /// the jobs that send them.
    queued_blobs: Mutex<HashMap<String, BTreeSet<u64>>>,
    /// Signalled as each queued blob is sent or given up.
    blob_done: Condvar,
    /// The newest queued upload of each entry and warm record, by path, which
    /// the proxy serves until the upstream has it.
    queued_records: Mutex<HashMap<String, u64>>,
    /// The paths of records being sent now, which another upload of the same
    /// path waits for, so that the upstream takes them in order.
    sending_records: Mutex<HashSet<String>>,
    record_sent: Condvar,
    /// Whether the upstream holds queued blobs, as asked for many at once.
    upstream_holds: Mutex<HashMap<String, bool>>,
    /// Whether the upstream turned a bundle away, as one that takes none.
    /// Bundles go to it as they came from then on, so that clients see its
    /// refusal and send their blobs one each.
    upstream_refuses_bundles: AtomicBool,
    /// Whether the upstream answered `POST blobs/fetch` with `404`, as one
    /// that sends blobs only one each. The proxy then answers it with the
    /// blobs it holds, and clients fetch the rest one each through it.
    upstream_fetches_one_each: AtomicBool,
    max_size: Option<u64>,
    /// Tokens the upstream accepted, which may read blobs held here.
    trusted: Mutex<HashSet<String>>,
    /// Tokens the upstream took a write from, whose uploads the proxy answers
    /// for and queues.
    writers: Mutex<HashSet<String>>,
    /// Held while blobs are evicted, so two connections do not both evict.
    evicting: Mutex<()>,
    size: AtomicU64,
    stats: Stats,
}

#[derive(Default)]
struct Stats {
    local_hits: AtomicU64,
    local_bytes: AtomicU64,
    upstream_requests: AtomicU64,
    upstream_bytes: AtomicU64,
    stored: AtomicU64,
    evicted: AtomicU64,
    queued: AtomicU64,
    uploaded: AtomicU64,
    dropped: AtomicU64,
    superseded: AtomicU64,
}

pub fn serve(options: Options) -> Result<()> {
    let proxy = Proxy::open(&options)?;
    let resumed = proxy.resume_queue()?;
    // Within a limit lowered since it last ran, the queue held.
    proxy.evict();
    for _ in 0..UPLOADERS {
        let proxy = proxy.clone();
        std::thread::spawn(move || proxy.upload_queued());
    }
    let listener = TcpListener::bind(&options.listen)
        .with_context(|| format!("cannot listen on {}", options.listen))?;
    eprintln!(
        "qk: store proxy for {} listening on http://{}, holding {} in {}, {resumed} uploads queued",
        proxy.upstream,
        listener.local_addr()?,
        bytes(proxy.size.load(Ordering::Relaxed)),
        options.directory.display()
    );
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let proxy = proxy.clone();
        std::thread::spawn(move || {
            let _ = connection(stream, &proxy);
        });
    }
    Ok(())
}

impl Proxy {
    /// The proxy keeping its blobs and queue in `options.directory`.
    fn open(options: &Options) -> Result<Arc<Self>> {
        let blobs = options.directory.join("blobs");
        let scratch = options.directory.join("tmp");
        let queue = options.directory.join("queue");
        let records = options.directory.join("records");
        fs::create_dir_all(&blobs).with_context(|| format!("cannot create {}", blobs.display()))?;
        fs::create_dir_all(&scratch)?;
        fs::create_dir_all(&queue)?;
        fs::create_dir_all(&records)?;
        // Partial downloads of an earlier run are no blob.
        for entry in fs::read_dir(&scratch)?.flatten() {
            let _ = fs::remove_file(entry.path());
        }
        let size = [&blobs, &records]
            .into_iter()
            .flat_map(|directory| fs::read_dir(directory).into_iter().flatten().flatten())
            .filter_map(|entry| entry.metadata().ok())
            .map(|metadata| metadata.len())
            .sum();
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .timeout_connect(Some(Duration::from_secs(10)))
            // A queued upload the upstream never answers holds an uploader no
            // longer than this.
            .timeout_recv_response(Some(Duration::from_secs(300)))
            .build()
            .into();
        let proxy = Arc::new(Proxy {
            agent,
            upstream: options.upstream.trim_end_matches('/').to_owned(),
            blobs,
            scratch,
            queue,
            records,
            next_job: AtomicU64::new(0),
            jobs: Mutex::new(VecDeque::new()),
            job_ready: Condvar::new(),
            queued_blobs: Mutex::new(HashMap::new()),
            blob_done: Condvar::new(),
            queued_records: Mutex::new(HashMap::new()),
            sending_records: Mutex::new(HashSet::new()),
            record_sent: Condvar::new(),
            upstream_holds: Mutex::new(HashMap::new()),
            upstream_refuses_bundles: AtomicBool::new(false),
            upstream_fetches_one_each: AtomicBool::new(false),
            index: Mutex::new(None),
            catching_up: Mutex::new(None),
            max_size: options.max_size,
            trusted: Mutex::new(HashSet::new()),
            writers: Mutex::new(HashSet::new()),
            evicting: Mutex::new(()),
            size: AtomicU64::new(size),
            stats: Stats::default(),
        });
        Ok(proxy)
    }
}

#[derive(Debug)]
struct Request {
    method: String,
    /// The path and query after the host, such as `/v4/blobs/<hash>`.
    target: String,
    authorization: Option<String>,
    content_type: Option<String>,
    body: Vec<u8>,
    close: bool,
}

/// Serves the requests of one connection until the client closes it.
fn connection(stream: TcpStream, proxy: &Proxy) -> Result<()> {
    stream.set_nodelay(true)?;
    // A client that stalls holds a thread no longer than this.
    stream.set_read_timeout(Some(IDLE))?;
    stream.set_write_timeout(Some(IDLE))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = io::BufWriter::new(stream);
    while let Some(request) = read_request(&mut reader)? {
        let started = Instant::now();
        let (status, source, sent) = handle(proxy, &request, &mut writer)?;
        writer.flush()?;
        eprintln!(
            "qk: {} {} {status} {source} {} {}ms",
            request.method,
            request.target,
            bytes(sent),
            started.elapsed().as_millis()
        );
        if request.close {
            break;
        }
    }
    Ok(())
}

/// One line of the request, without its line ending; `None` at the end of
/// the stream. A line longer than [`MAX_LINE`] is refused.
fn read_line(reader: &mut impl BufRead) -> Result<Option<String>> {
    let mut line = String::new();
    if reader.by_ref().take(MAX_LINE + 1).read_line(&mut line)? == 0 {
        return Ok(None);
    }
    if !line.ends_with('\n') {
        bail!("a request line is too long or cut short");
    }
    Ok(Some(line.trim_end_matches(['\r', '\n']).to_owned()))
}

/// The next request on a connection. Framing that two readers could take
/// differently is refused, as a front proxy might frame the same bytes
/// otherwise and smuggle a request past it: conflicting lengths, a length
/// beside a transfer encoding, or any transfer encoding but `chunked`.
fn read_request(reader: &mut impl BufRead) -> Result<Option<Request>> {
    let Some(line) = read_line(reader)? else {
        return Ok(None);
    };
    let mut parts = line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        bail!("malformed request line");
    };
    // Only a path: the target is appended to the upstream's address, where
    // anything else could name another host.
    if !target.starts_with('/') {
        bail!("unsupported request target {target:?}");
    }
    let (method, target) = (method.to_owned(), target.to_owned());
    let mut length: Option<usize> = None;
    let mut encoding: Option<String> = None;
    let mut authorization = None;
    let mut content_type = None;
    let mut close = false;
    for count in 0.. {
        if count == MAX_HEADERS {
            bail!("too many headers");
        }
        let header = read_line(reader)?.context("the request ends in its headers")?;
        if header.is_empty() {
            break;
        }
        // A line without a colon, as a folded continuation, or a name with
        // whitespace before its colon, could be read otherwise by a proxy in
        // front, which would frame the body differently (RFC 9112 5.1).
        let Some((name, value)) = header.split_once(':') else {
            bail!("malformed header line");
        };
        if name.is_empty() || name.bytes().any(|byte| byte.is_ascii_whitespace()) {
            bail!("malformed header name {name:?}");
        }
        let value = value.trim();
        match name.to_ascii_lowercase().as_str() {
            "content-length" => {
                let parsed: usize = value.parse().context("bad content-length")?;
                if length.is_some_and(|known| known != parsed) {
                    bail!("conflicting content-length headers");
                }
                length = Some(parsed);
            }
            "transfer-encoding" => {
                if encoding.is_some() {
                    bail!("more than one transfer-encoding header");
                }
                encoding = Some(value.to_owned());
            }
            "authorization" => authorization = Some(value.to_owned()),
            "content-type" => content_type = Some(value.to_owned()),
            "connection" => close = value.eq_ignore_ascii_case("close"),
            _ => {}
        }
    }
    let body = match (encoding, length) {
        (Some(_), Some(_)) => bail!("both content-length and transfer-encoding"),
        (Some(encoding), None) if encoding.eq_ignore_ascii_case("chunked") => read_chunked(reader)?,
        (Some(encoding), None) => bail!("unsupported transfer-encoding {encoding:?}"),
        (None, length) => {
            let length = length.unwrap_or(0);
            if length > MAX_BODY {
                bail!("a request body of {length} bytes is larger than the proxy takes");
            }
            read_body(reader, length)?
        }
    };
    Ok(Some(Request {
        method,
        target,
        authorization,
        content_type,
        body,
        close,
    }))
}

/// Exactly `length` bytes, the buffer growing only as they arrive.
fn read_body(reader: &mut impl BufRead, length: usize) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    reader.by_ref().take(length as u64).read_to_end(&mut body)?;
    if body.len() != length {
        bail!("the request body ends early");
    }
    Ok(body)
}

fn read_chunked(reader: &mut impl BufRead) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let line = read_line(reader)?.context("the request ends in a chunk")?;
        let size = usize::from_str_radix(line.trim().split(';').next().unwrap_or(""), 16)
            .context("bad chunk size")?;
        if size == 0 {
            // Trailers, then the blank line that ends them.
            loop {
                let line = read_line(reader)?.context("the request ends in its trailers")?;
                if line.trim().is_empty() {
                    return Ok(body);
                }
            }
        }
        if body.len().saturating_add(size) > MAX_BODY {
            bail!("a chunked request body is larger than the proxy takes");
        }
        body.extend(read_body(reader, size)?);
        read_line(reader)?.context("the request ends after a chunk")?;
    }
}

/// Answers one request, and says where the answer came from and how many
/// bytes of body it sent.
fn handle(
    proxy: &Proxy,
    request: &Request,
    writer: &mut impl Write,
) -> Result<(u16, &'static str, u64)> {
    let path = record_path(&request.target);
    let record = path.starts_with("/v4/entries/") || path.starts_with("/v4/warm/");
    // Many small blobs as one upload body, which the proxy takes as a record
    // whose blobs records citing them wait for.
    let bundle = path.starts_with("/v4/bundles/");
    if request.method == "GET" && path == "/proxy/stats" {
        let body = proxy.stats_json().into_bytes();
        head(
            writer,
            200,
            &[("content-type", "application/json")],
            Some(body.len() as u64),
        )?;
        writer.write_all(&body)?;
        return Ok((200, "proxy", body.len() as u64));
    }
    let blob = path
        .strip_prefix("/v4/blobs/")
        .filter(|hash| hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()));
    if let Some(hash) = blob
        && matches!(request.method.as_str(), "GET" | "HEAD")
        && proxy.trusts(request)
        && let Ok(file) = File::open(proxy.blobs.join(hash))
    {
        let length = file.metadata()?.len();
        // A hit counts as a use, which eviction goes by.
        let _ = file.set_modified(SystemTime::now());
        head(
            writer,
            200,
            &[("content-type", "application/zstd")],
            Some(length),
        )?;
        let mut sent = 0;
        if request.method == "GET" {
            sent = io::copy(&mut BufReader::new(file), writer)?;
            proxy.stats.local_hits.fetch_add(1, Ordering::Relaxed);
            proxy.stats.local_bytes.fetch_add(sent, Ordering::Relaxed);
        }
        return Ok((200, "local", sent));
    }
    // Many blobs at once, from those held here once the upstream has sent
    // those it lacks. A body that does not parse goes upstream as it came.
    if request.method == "POST"
        && path == "/v4/blobs/fetch"
        && proxy.trusts(request)
        && let Some(asked) = fetch_query(&request.body)
    {
        let sent = proxy.fetch_blobs(&asked, request, writer)?;
        return Ok((200, "proxy", sent));
    }
    // An entry or warm record still queued is the newest; the upstream has
    // an older one or none. Once sent on, it is kept and served too.
    if record
        && matches!(request.method.as_str(), "GET" | "HEAD")
        && proxy.trusts(request)
        && let Some((pack, source)) = proxy
            .queued_pack(path)
            .map(|pack| (pack, "queued"))
            .or_else(|| proxy.kept_pack(path).map(|pack| (pack, "kept")))
    {
        head(
            writer,
            200,
            &[("content-type", "application/x-qk-pack")],
            Some(pack.len() as u64),
        )?;
        let mut sent = 0;
        if request.method == "GET" {
            writer.write_all(&pack)?;
            sent = pack.len() as u64;
            proxy.stats.local_hits.fetch_add(1, Ordering::Relaxed);
            proxy.stats.local_bytes.fetch_add(sent, Ordering::Relaxed);
        }
        return Ok((200, source, sent));
    }
    // The whole index, which a client with no cursor asks for, from the copy
    // brought up to date with what changed upstream since.
    if request.method == "GET"
        && request.target == "/v4/index"
        && proxy.trusts(request)
        && let Some(authorization) = &request.authorization
        && let Ok((text, cursor)) = proxy.index_listing(authorization)
    {
        let text: &str = &text;
        head(
            writer,
            200,
            &[
                ("content-type", "text/plain; charset=utf-8"),
                ("qk-index-cursor", cursor.as_str()),
            ],
            Some(text.len() as u64),
        )?;
        writer.write_all(text.as_bytes())?;
        proxy.stats.local_hits.fetch_add(1, Ordering::Relaxed);
        proxy
            .stats
            .local_bytes
            .fetch_add(text.len() as u64, Ordering::Relaxed);
        return Ok((200, "proxy", text.len() as u64));
    }
    // A client asks before uploading a blob. Uploading one the upstream has
    // costs only the local network, and the proxy asks the upstream itself
    // before sending it on.
    if blob.is_some() && request.method == "HEAD" && proxy.trusts(request) {
        head(writer, 404, &[], Some(0))?;
        return Ok((404, "local", 0));
    }
    // An upload from a token the upstream accepted is kept, queued and
    // answered at once. Others go upstream as they came, the answer being
    // the upstream's to give.
    if request.method == "PUT" && proxy.writes(request) && (blob.is_some() || record || bundle) {
        let queued = match blob {
            Some(hash) => proxy.queue_blob(request, hash).unwrap_or(false),
            // A bundle is queued first, which holds its blobs against
            // eviction while they are kept; one that does not parse, or is
            // past what a store server takes, goes upstream as it came, for
            // the upstream to refuse.
            None if bundle && proxy.upstream_refuses_bundles.load(Ordering::Relaxed) => false,
            None if bundle => match bundle_blobs(path, &request.body) {
                Some(blobs) => {
                    let queued = proxy.enqueue(request, None).is_ok();
                    if queued {
                        for (hash, frame) in blobs {
                            proxy.keep(&hash, frame);
                        }
                    }
                    queued
                }
                None => false,
            },
            None => {
                for (hash, frame) in inline_blobs(&request.body).unwrap_or_default() {
                    proxy.keep(&hash, frame);
                }
                proxy.enqueue(request, None).is_ok()
            }
        };
        if queued {
            head(writer, 204, &[], Some(0))?;
            return Ok((204, "queued", 0));
        }
    }
    let keep = match (request.method.as_str(), blob) {
        ("GET", Some(hash)) => Some(hash),
        _ => None,
    };
    let (status, sent) = proxy.forward(request, writer, keep)?;
    if request.method == "PUT"
        && (200..300).contains(&status)
        && let Some(token) = &request.authorization
    {
        proxy.writers.lock().unwrap().insert(token.clone());
    }
    // An upload the upstream took is kept too, for the runners that fetch it:
    // a blob on its own, or those a result or warm record carries inline.
    // The client has its answer first.
    if request.method == "PUT" && status == 204 {
        writer.flush()?;
        if let Some(hash) = blob {
            proxy.keep(hash, &request.body);
        } else if record || bundle {
            for (hash, frame) in inline_blobs(&request.body).unwrap_or_default() {
                proxy.keep(&hash, frame);
            }
            // An older one kept is no longer what the upstream holds. Not
            // replaced, as two such uploads may be answered in either order;
            // the next read goes upstream. An entry the upstream committed
            // never changes, so it is kept.
            if path.starts_with("/v4/warm/") {
                proxy.forget_record(path);
            } else if path.starts_with("/v4/entries/") {
                proxy.keep_body(path, &request.body);
            }
        }
    }

    Ok((status, "upstream", sent))
}

impl Proxy {
    fn trusts(&self, request: &Request) -> bool {
        request
            .authorization
            .as_ref()
            .is_some_and(|token| self.trusted.lock().unwrap().contains(token))
    }

    /// Whether the upstream has taken a write from the request's token. A
    /// token that may only read is trusted all the same, and the upstream
    /// would refuse its uploads after the proxy had answered for them.
    fn writes(&self, request: &Request) -> bool {
        request
            .authorization
            .as_ref()
            .is_some_and(|token| self.writers.lock().unwrap().contains(token))
    }

    /// Sends the request upstream and passes its answer back, keeping the body
    /// as the blob `keep` when the upstream has it.
    fn forward(
        &self,
        request: &Request,
        writer: &mut impl Write,
        keep: Option<&str>,
    ) -> Result<(u16, u64)> {
        self.stats.upstream_requests.fetch_add(1, Ordering::Relaxed);
        let mut builder = ureq::http::Request::builder()
            .method(request.method.as_str())
            .uri(format!("{}{}", self.upstream, request.target));
        if let Some(token) = &request.authorization {
            builder = builder.header("authorization", token);
        }
        if let Some(content_type) = &request.content_type {
            builder = builder.header("content-type", content_type);
        }
        let response = self.agent.run(builder.body(request.body.clone())?);
        let mut response = match response {
            Ok(response) => response,
            Err(error) => {
                let body = format!(r#"{{"error":"busy","message":"upstream: {error}"}}"#);
                head(
                    writer,
                    503,
                    &[("content-type", "application/json"), ("retry-after", "5")],
                    Some(body.len() as u64),
                )?;
                writer.write_all(body.as_bytes())?;
                return Ok((503, body.len() as u64));
            }
        };
        let status = response.status().as_u16();
        if status != 401
            && let Some(token) = &request.authorization
        {
            self.trusted.lock().unwrap().insert(token.clone());
        }
        let headers: Vec<(&str, String)> = PASSED_HEADERS
            .iter()
            .filter_map(|name| {
                response
                    .headers()
                    .get(*name)
                    .and_then(|value| value.to_str().ok())
                    .map(|value| (*name, value.to_owned()))
            })
            .collect();
        let headers: Vec<(&str, &str)> = headers
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect();
        if request.method == "HEAD" {
            head(writer, status, &headers, Some(0))?;
            return Ok((status, 0));
        }
        let mut body = response.body_mut().as_reader();
        let mut copy = match keep {
            Some(_) if status == 200 => tempfile::NamedTempFile::new_in(&self.scratch).ok(),
            _ => None,
        };
        // Sent in chunks as it arrives: the upstream's length may be unknown.
        head(writer, status, &headers, None)?;
        let mut buffer = vec![0; 256 << 10];
        let mut sent = 0;
        loop {
            let read = body.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            write!(writer, "{read:x}\r\n")?;
            writer.write_all(&buffer[..read])?;
            writer.write_all(b"\r\n")?;
            if let Some(file) = &mut copy
                && file.write_all(&buffer[..read]).is_err()
            {
                copy = None;
            }
            sent += read as u64;
        }
        writer.write_all(b"0\r\n\r\n")?;
        // The client has its answer before the blob is checked and kept.
        writer.flush()?;
        self.stats.upstream_bytes.fetch_add(sent, Ordering::Relaxed);
        if let (Some(hash), Some(file)) = (keep, copy) {
            self.store(hash, file);
        }
        Ok((status, sent))
    }

    /// Whether the proxy holds the blob already, which then counts as a use.
    fn held(&self, hash: &str) -> bool {
        File::options()
            .write(true)
            .open(self.blobs.join(hash))
            .is_ok_and(|file| file.set_modified(SystemTime::now()).is_ok())
    }

    /// Keeps a blob the proxy holds in memory, answering whether it holds it.
    fn keep(&self, hash: &str, frame: &[u8]) -> bool {
        if self.held(hash) {
            return true;
        }
        let Ok(mut file) = tempfile::NamedTempFile::new_in(&self.scratch) else {
            return false;
        };
        file.write_all(frame).is_ok() && self.store(hash, file)
    }

    /// Moves a downloaded frame into place once it decodes to its hash: the
    /// proxy serves what it keeps without asking the upstream again. Answers
    /// whether it holds the blob.
    fn store(&self, hash: &str, file: tempfile::NamedTempFile) -> bool {
        // Kept meanwhile, as by another connection: counted once.
        if self.held(hash) {
            return true;
        }
        let verified = File::open(file.path()).ok().is_some_and(|frame| {
            let mut hasher = blake3::Hasher::new();
            zstd::stream::read::Decoder::new(frame)
                .and_then(|mut decoder| io::copy(&mut decoder, &mut hasher))
                .is_ok()
                && hasher.finalize().to_hex().as_str() == hash
        });
        if !verified {
            return false;
        }
        let length = file
            .as_file()
            .metadata()
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        if file.persist(self.blobs.join(hash)).is_err() {
            return false;
        }
        self.stats.stored.fetch_add(1, Ordering::Relaxed);
        let size = self.size.fetch_add(length, Ordering::Relaxed) + length;
        if self.max_size.is_some_and(|max| size > max) {
            self.evict();
        }
        true
    }

    /// Removes the least recently used blobs until the proxy holds at most 90%
    /// of its limit.
    fn evict(&self) {
        let Some(max) = self.max_size else { return };
        let Ok(_guard) = self.evicting.try_lock() else {
            return;
        };
        // Blobs and kept records together, the least recently used first.
        let mut blobs: Vec<(SystemTime, u64, PathBuf)> = [&self.blobs, &self.records]
            .into_iter()
            .flat_map(|directory| fs::read_dir(directory).into_iter().flatten().flatten())
            .filter_map(|entry| {
                let metadata = entry.metadata().ok()?;
                Some((metadata.modified().ok()?, metadata.len(), entry.path()))
            })
            .collect();
        blobs.sort();
        let mut size: u64 = blobs.iter().map(|(_, length, _)| length).sum();
        let target = max / 10 * 9;
        for (_, length, path) in blobs {
            if size <= target {
                break;
            }
            // Its upload is still to be sent from here. Held while the blob is
            // removed, so that an upload queuing it meanwhile either finds it
            // gone, and keeps its own, or holds it before it can go.
            let queued = self.queued_blobs.lock().unwrap();
            let held = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| queued.contains_key(name));
            if held {
                continue;
            }
            if fs::remove_file(&path).is_ok() {
                size -= length;
                self.stats.evicted.fetch_add(1, Ordering::Relaxed);
            }
            drop(queued);
        }
        self.size.store(size, Ordering::Relaxed);
    }

    /// Answers `POST blobs/fetch` for `asked` with the blobs held here, after
    /// fetching from the upstream, many at once, those it lacks. The answer
    /// is the store server's format, within its budget; what it leaves out,
    /// the client asks for again.
    fn fetch_blobs(
        &self,
        asked: &[String],
        request: &Request,
        writer: &mut impl Write,
    ) -> Result<u64> {
        let lacking: Vec<&String> = asked.iter().filter(|hash| !self.held(hash)).collect();
        if !lacking.is_empty()
            && !self.upstream_fetches_one_each.load(Ordering::Relaxed)
            && let Some(token) = &request.authorization
        {
            // A failure leaves the blobs out, for the client to fetch one each.
            let _ = self.fetch_upstream(&lacking, token);
        }
        let mut listed = Vec::new();
        let mut total = 0;
        for hash in asked {
            let Ok(file) = File::open(self.blobs.join(hash)) else {
                continue;
            };
            let length = file.metadata()?.len();
            if total + length > FETCH_BUDGET {
                continue;
            }
            total += length;
            listed.push((hash, file, length));
        }
        let mut table = Vec::with_capacity(8 + 40 * listed.len());
        table.extend_from_slice(b"qkb1");
        table.extend_from_slice(&(listed.len() as u32).to_le_bytes());
        for (hash, _, length) in &listed {
            table.extend(hash_bytes(hash).context("a held blob's name is not a hash")?);
            table.extend_from_slice(&length.to_le_bytes());
        }
        head(
            writer,
            200,
            &[("content-type", "application/x-qk-blobs")],
            Some(table.len() as u64 + total),
        )?;
        writer.write_all(&table)?;
        let served = listed.len() as u64;
        for (hash, file, length) in listed {
            // Held open, so eviction meanwhile does not cut it short.
            if io::copy(&mut BufReader::new(file).take(length), writer)? != length {
                bail!("held blob {hash} changed while it was sent");
            }
        }
        self.stats.local_hits.fetch_add(served, Ordering::Relaxed);
        self.stats.local_bytes.fetch_add(total, Ordering::Relaxed);
        Ok(table.len() as u64 + total)
    }

    /// Fetches `blobs` from the upstream many at once and keeps those it sends.
    fn fetch_upstream(&self, blobs: &[&String], token: &str) -> Result<()> {
        self.stats.upstream_requests.fetch_add(1, Ordering::Relaxed);
        let mut response = self
            .agent
            .post(format!("{}/v4/blobs/fetch", self.upstream))
            .header("authorization", token)
            .header("content-type", "application/json")
            .send(serde_json::json!({ "blobs": blobs }).to_string())?;
        match response.status().as_u16() {
            200 => {}
            404 => {
                self.upstream_fetches_one_each
                    .store(true, Ordering::Relaxed);
                return Ok(());
            }
            status => bail!("the upstream answered POST blobs/fetch with {status}"),
        }
        let mut body = BufReader::new(response.body_mut().as_reader());
        let mut start = [0; 8];
        body.read_exact(&mut start)?;
        if &start[..4] != b"qkb1" {
            bail!("the upstream's blobs are not a qkb1 answer");
        }
        let count = u32::from_le_bytes(start[4..].try_into()?) as usize;
        if count > blobs.len() {
            bail!("the upstream listed more blobs than were asked for");
        }
        let mut listed = Vec::with_capacity(count);
        for _ in 0..count {
            let mut entry = [0; 40];
            body.read_exact(&mut entry)?;
            let hash: String = entry[..32]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            if !blobs.iter().any(|asked| **asked == hash) {
                bail!("the upstream listed {hash}, which was not asked for");
            }
            listed.push((hash, u64::from_le_bytes(entry[32..].try_into()?)));
        }
        let mut received = 8 + 40 * count as u64;
        for (hash, length) in listed {
            let mut file = tempfile::NamedTempFile::new_in(&self.scratch)?;
            if io::copy(&mut (&mut body).take(length), &mut file)? != length {
                bail!("the upstream's blobs end early");
            }
            received += length;
            // Checked against its hash before it is kept.
            self.store(&hash, file);
        }
        self.stats
            .upstream_bytes
            .fetch_add(received, Ordering::Relaxed);
        Ok(())
    }

    fn stats_json(&self) -> String {
        let stats = &self.stats;
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        serde_json::json!({
            "localHits": load(&stats.local_hits),
            "localBytes": load(&stats.local_bytes),
            "upstreamRequests": load(&stats.upstream_requests),
            "upstreamBytes": load(&stats.upstream_bytes),
            "stored": load(&stats.stored),
            "evicted": load(&stats.evicted),
            "queued": load(&stats.queued),
            "uploaded": load(&stats.uploaded),
            "dropped": load(&stats.dropped),
            "superseded": load(&stats.superseded),
            "size": load(&self.size),
        })
        .to_string()
    }
}

/// An upload the proxy answered for and sends upstream in the background.
#[derive(Serialize, Deserialize)]
struct Job {
    /// The path it is sent to, such as `/v4/blobs/<hash>`.
    target: String,
    authorization: String,
    content_type: Option<String>,
    /// The blob it sends, which the proxy holds; a record's body is kept
    /// beside the job instead.
    blob: Option<String>,
}

/// One upload's turn to send the record at `path`.
struct RecordTurn<'a> {
    proxy: &'a Proxy,
    path: String,
}

impl Drop for RecordTurn<'_> {
    fn drop(&mut self) {
        self.proxy
            .sending_records
            .lock()
            .unwrap()
            .remove(&self.path);
        self.proxy.record_sent.notify_all();
    }
}

/// What sending a queued upload came to.
enum Sent {
    Done(u16),
    /// The upstream will not take it, as for a token it refuses.
    Refused(String),
    /// The upstream lacked blobs the record cites; those the proxy held are
    /// sent now, and the record is to be sent again.
    Missing,
    /// It may take it later, as when it could not be reached.
    Later(String),
}

impl Proxy {
    fn job_path(&self, id: u64) -> PathBuf {
        self.queue.join(format!("{id:020}.json"))
    }

    fn body_path(&self, id: u64) -> PathBuf {
        self.queue.join(format!("{id:020}.body"))
    }

    /// Queues `request` to be sent upstream, with the body kept beside it
    /// unless it sends the held `blob`. On disk before the client is answered,
    /// so a proxy that stops sends it once started again.
    fn enqueue(&self, request: &Request, blob: Option<&str>) -> Result<()> {
        if let Some(hash) = blob
            && self.queued_blobs.lock().unwrap().contains_key(hash)
        {
            return Ok(());
        }
        let job = Job {
            target: request.target.clone(),
            authorization: request
                .authorization
                .clone()
                .context("an upload without a token")?,
            content_type: request.content_type.clone(),
            blob: blob.map(str::to_owned),
        };
        let id = self.next_job.fetch_add(1, Ordering::Relaxed);
        self.write_job(id, &job, blob.is_none().then_some(&request.body[..]))?;
        self.admit(id, &job);
        Ok(())
    }

    /// Keeps the blob `request` uploads and queues sending it on, answering
    /// whether it is queued. The blob is held against eviction from before it
    /// is kept, as keeping it can evict and so can another connection meanwhile,
    /// which would lose an upload the client was told the proxy took.
    fn queue_blob(&self, request: &Request, hash: &str) -> Result<bool> {
        let id = {
            let mut queued = self.queued_blobs.lock().unwrap();
            if queued.contains_key(hash) {
                // Queued already, and so held.
                drop(queued);
                return Ok(self.keep(hash, &request.body));
            }
            let id = self.next_job.fetch_add(1, Ordering::Relaxed);
            queued.entry(hash.to_owned()).or_default().insert(id);
            id
        };
        let unpin = || {
            let mut queued = self.queued_blobs.lock().unwrap();
            if let Some(ids) = queued.get_mut(hash) {
                ids.remove(&id);
                if ids.is_empty() {
                    queued.remove(hash);
                }
            }
            drop(queued);
            self.blob_done.notify_all();
        };
        if !self.keep(hash, &request.body) {
            unpin();
            return Ok(false);
        }
        let job = Job {
            target: request.target.clone(),
            authorization: request
                .authorization
                .clone()
                .context("an upload without a token")?,
            content_type: request.content_type.clone(),
            blob: Some(hash.to_owned()),
        };
        if let Err(error) = self.write_job(id, &job, None) {
            unpin();
            return Err(error);
        }
        self.admit_held(id, &job);
        Ok(true)
    }

    /// Writes the job file of `id`, and the record body it sends unless it
    /// sends a held blob. Only its owner may read a job file, which holds the
    /// client's token.
    fn write_job(&self, id: u64, job: &Job, body: Option<&[u8]>) -> Result<()> {
        let write = |path: PathBuf, bytes: &[u8]| -> Result<()> {
            let mut file = tempfile::NamedTempFile::new_in(&self.queue)?;
            file.write_all(bytes)?;
            file.persist(path)?;
            Ok(())
        };
        if let Some(body) = body {
            write(self.body_path(id), body)?;
        }
        write(self.job_path(id), &serde_json::to_vec(job)?)
    }

    fn admit(&self, id: u64, job: &Job) {
        let mut queued = self.queued_blobs.lock().unwrap();
        for hash in self.job_blobs(id, job) {
            queued.entry(hash).or_default().insert(id);
        }
        drop(queued);
        self.admit_held(id, job);
    }

    /// The blobs job `id` sends: the one it uploads on its own, or those its
    /// bundle carries. A record citing them waits for it.
    fn job_blobs(&self, id: u64, job: &Job) -> Vec<String> {
        match &job.blob {
            Some(hash) => vec![hash.clone()],
            None if is_bundle(&job.target) => fs::read(self.body_path(id))
                .ok()
                .and_then(|body| {
                    inline_blobs(&body)
                        .map(|blobs| blobs.into_iter().map(|(hash, _)| hash).collect())
                })
                .unwrap_or_default(),
            None => Vec::new(),
        }
    }

    /// Queues `id`, whose blob, if it sends one, is held already.
    fn admit_held(&self, id: u64, job: &Job) {
        // Queued in order, so the last admitted is the newest.
        if job.blob.is_none() && !is_bundle(&job.target) {
            self.queued_records
                .lock()
                .unwrap()
                .insert(record_path(&job.target).to_owned(), id);
        }
        self.stats.queued.fetch_add(1, Ordering::Relaxed);
        self.jobs.lock().unwrap().push_back(id);
        self.job_ready.notify_one();
    }

    /// Queues again, in their order, the uploads an earlier proxy left, and
    /// answers how many.
    fn resume_queue(&self) -> Result<usize> {
        let mut ids = Vec::new();
        for entry in fs::read_dir(&self.queue)?.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            match name
                .strip_suffix(".json")
                .and_then(|id| id.parse::<u64>().ok())
            {
                Some(id) => ids.push(id),
                // Bodies whose jobs are gone, and files never put in place.
                None if name.ends_with(".body")
                    && self.queue.join(name.replace(".body", ".json")).is_file() => {}
                None => {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
        ids.sort_unstable();
        let mut resumed = 0;
        for &id in &ids {
            let job = fs::read(self.job_path(id))
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Job>(&bytes).ok());
            match job {
                Some(job) if job.blob.is_some() || self.body_path(id).is_file() => {
                    self.admit(id, &job);
                    resumed += 1;
                }
                _ => {
                    let _ = fs::remove_file(self.job_path(id));
                    let _ = fs::remove_file(self.body_path(id));
                }
            }
        }
        let next = ids.last().map_or(0, |id| id + 1);
        self.next_job.store(next, Ordering::Relaxed);
        Ok(resumed)
    }

    /// Sends queued uploads upstream, one after another, for as long as the
    /// proxy runs.
    fn upload_queued(&self) {
        loop {
            let id = {
                let mut jobs = self.jobs.lock().unwrap();
                loop {
                    if let Some(id) = jobs.pop_front() {
                        break id;
                    }
                    jobs = self.job_ready.wait(jobs).unwrap();
                }
            };
            self.upload(id);
        }
    }

    /// Sends one queued upload, again after a pause while the upstream may
    /// take it later, and removes it once sent or refused.
    fn upload(&self, id: u64) {
        let job = fs::read(self.job_path(id))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Job>(&bytes).ok());
        if let Some(job) = &job {
            if job.blob.is_none() {
                self.await_blobs(id);
            }
            let started = Instant::now();
            let mut pause = Duration::from_secs(1);
            let mut missing = 0;
            let outcome = loop {
                // A record is sent on its path's turn, unless a newer upload
                // of it has been queued since, which replaces it. A bundle is
                // named by what it holds, so it takes no turn: one sent again
                // is the same, and records waiting for the first keep waiting.
                let turn = match job.blob {
                    Some(_) => None,
                    None if is_bundle(&job.target) => None,
                    None => match self.record_turn(record_path(&job.target), id) {
                        Some(turn) => Some(turn),
                        None => {
                            self.stats.superseded.fetch_add(1, Ordering::Relaxed);
                            break "superseded by a newer upload".into();
                        }
                    },
                };
                let sent = self.send(job, id);
                // An entry or warm record sent upstream is kept, so the proxy
                // serves it once it is no longer queued; on its turn, so that
                // a newer upload of it is kept after. Not an entry another run
                // committed first, which may hold other outputs than this one.
                if matches!(sent, Sent::Done(status) if status != 409)
                    && job.blob.is_none()
                    && (record_path(&job.target).starts_with("/v4/warm/")
                        || record_path(&job.target).starts_with("/v4/entries/"))
                {
                    self.keep_record(record_path(&job.target), &self.body_path(id));
                }
                drop(turn);
                match sent {
                    Sent::Done(status) => {
                        self.stats.uploaded.fetch_add(1, Ordering::Relaxed);
                        break status.to_string();
                    }
                    Sent::Refused(why) => {
                        self.stats.dropped.fetch_add(1, Ordering::Relaxed);
                        break format!("dropped: {why}");
                    }
                    Sent::Missing => {
                        missing += 1;
                        if missing == MISSING_ROUNDS {
                            self.stats.dropped.fetch_add(1, Ordering::Relaxed);
                            break "dropped: the upstream still lacks blobs it cites".into();
                        }
                    }
                    Sent::Later(why) => {
                        eprintln!(
                            "qk: queued PUT {} failed, again in {}s: {why}",
                            job.target,
                            pause.as_secs()
                        );
                        std::thread::sleep(pause);
                        pause = (pause * 2).min(RETRY_PAUSE);
                    }
                }
            };
            eprintln!(
                "qk: queued PUT {} {outcome} {}ms",
                job.target,
                started.elapsed().as_millis()
            );
            let blobs = self.job_blobs(id, job);
            if !blobs.is_empty() {
                let mut queued = self.queued_blobs.lock().unwrap();
                let mut sent_on = Vec::new();
                for hash in &blobs {
                    if let Some(ids) = queued.get_mut(hash) {
                        ids.remove(&id);
                        if ids.is_empty() {
                            queued.remove(hash);
                            sent_on.push(hash);
                        }
                    }
                }
                drop(queued);
                let mut holds = self.upstream_holds.lock().unwrap();
                for hash in sent_on {
                    holds.remove(hash);
                }
                drop(holds);
                self.blob_done.notify_all();
            }
            if job.blob.is_none() {
                let path = record_path(&job.target);
                let mut records = self.queued_records.lock().unwrap();
                if records.get(path) == Some(&id) {
                    records.remove(path);
                }
            }
        }
        let _ = fs::remove_file(self.job_path(id));
        let _ = fs::remove_file(self.body_path(id));
        self.stats.queued.fetch_sub(1, Ordering::Relaxed);
    }

    /// Waits until the jobs queued before the record queued as `id` that send
    /// blobs it cites stored apart are done, so the upstream does not refuse
    /// it for lacking them. A client uploads those before the record, and the
    /// queue is taken in order, so other uploaders have those jobs already. A
    /// job queued after the record, as for the same blob uploaded again, may
    /// wait for an uploader the record holds, so it is not waited for.
    fn await_blobs(&self, id: u64) {
        let Ok(body) = fs::read(self.body_path(id)) else {
            return;
        };
        let apart = apart_blobs(&body).unwrap_or_default();
        let mut queued = self.queued_blobs.lock().unwrap();
        let ahead = |queued: &HashMap<String, BTreeSet<u64>>| {
            apart.iter().any(|hash| {
                queued
                    .get(hash)
                    .and_then(|ids| ids.first())
                    .is_some_and(|first| *first < id)
            })
        };
        while ahead(&queued) {
            queued = self.blob_done.wait(queued).unwrap();
        }
    }

    /// Waits for no other upload of the record at `path` to be on its way,
    /// and takes the turn, which ends when it is dropped; or answers `None`
    /// once `id` is not the newest upload of it, as when a newer one is
    /// queued, or was sent and so left the queue.
    fn record_turn(&self, path: &str, id: u64) -> Option<RecordTurn<'_>> {
        let mut sending = self.sending_records.lock().unwrap();
        loop {
            if self.queued_records.lock().unwrap().get(path) != Some(&id) {
                return None;
            }
            if sending.insert(path.to_owned()) {
                return Some(RecordTurn {
                    proxy: self,
                    path: path.to_owned(),
                });
            }
            sending = self.record_sent.wait(sending).unwrap();
        }
    }

    /// Sends each blob the bundle of job `id` carries on its own, as an
    /// upstream that takes no bundles has them.
    fn send_bundled(&self, job: &Job, id: u64) -> Sent {
        let Ok(body) = fs::read(self.body_path(id)) else {
            return Sent::Refused("its body is gone".into());
        };
        for (hash, frame) in inline_blobs(&body).unwrap_or_default() {
            let file = tempfile::NamedTempFile::new_in(&self.scratch).and_then(|mut file| {
                file.write_all(frame)?;
                file.reopen()
            });
            let Ok(file) = file else {
                return Sent::Later(format!("blob {hash} could not be written to send"));
            };
            let response = self.put(
                &format!("/v4/blobs/{hash}"),
                &job.authorization,
                Some("application/zstd"),
                file,
            );
            match response.map(|response| response.status().as_u16()) {
                Ok(200..=299) => {}
                Ok(status) => return sent(status),
                Err(error) => return Sent::Later(error.to_string()),
            }
        }
        Sent::Done(204)
    }

    fn send(&self, job: &Job, id: u64) -> Sent {
        if let Some(hash) = &job.blob {
            return self.send_blob(hash, &job.authorization, true);
        }
        let Ok(body) = File::open(self.body_path(id)) else {
            return Sent::Refused("its body is gone".into());
        };
        let response = self.put(
            &job.target,
            &job.authorization,
            job.content_type.as_deref(),
            body,
        );
        let mut response = match response {
            Ok(response) => response,
            Err(error) => return Sent::Later(error.to_string()),
        };
        match response.status().as_u16() {
            // An entry another run committed first.
            409 if job.target.starts_with("/v4/entries/") => Sent::Done(409),
            // An upstream that takes no bundles takes their blobs one each.
            404 if is_bundle(&job.target) => {
                self.upstream_refuses_bundles.store(true, Ordering::Relaxed);
                self.send_bundled(job, id)
            }
            422 => {
                let answer: serde_json::Value = response
                    .body_mut()
                    .read_to_string()
                    .ok()
                    .and_then(|text| serde_json::from_str(&text).ok())
                    .unwrap_or(serde_json::Value::Null);
                let missing = answer["blobs"].as_array().cloned().unwrap_or_default();
                for hash in missing.iter().filter_map(serde_json::Value::as_str) {
                    let valid =
                        hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit());
                    if !valid || !self.blobs.join(hash).is_file() {
                        return Sent::Refused(format!("the upstream lacks blob {hash}"));
                    }
                    // The upstream just said it lacks it, whatever it said before.
                    match self.send_blob(hash, &job.authorization, false) {
                        Sent::Done(_) => {}
                        other => return other,
                    }
                }
                Sent::Missing
            }
            status => sent(status),
        }
    }

    /// Sends the held blob `hash`, unless asking the upstream finds it has it.
    fn send_blob(&self, hash: &str, authorization: &str, ask: bool) -> Sent {
        let target = format!("/v4/blobs/{hash}");
        if ask {
            match self.upstream_has(hash, authorization) {
                Ok(Ok(true)) => return Sent::Done(200),
                Ok(Ok(false)) => {}
                Ok(Err(status)) => return sent(status),
                Err(error) => return Sent::Later(error.to_string()),
            }
        }
        let Ok(file) = File::open(self.blobs.join(hash)) else {
            return Sent::Refused("the blob is gone".into());
        };
        match self.put(&target, authorization, Some("application/zstd"), file) {
            Ok(response) => sent(response.status().as_u16()),
            Err(error) => Sent::Later(error.to_string()),
        }
    }

    /// Whether the upstream has the blob `hash`. Asked for many queued blobs at
    /// once, so most of a queue's blobs cost no request of their own; an
    /// upstream without `POST blobs/missing` is asked about each.
    ///
    /// `Ok(Err(status))` when the upstream answered with a status other than
    /// a yes or a no, as for a token it refuses, which the caller weighs as it
    /// does an upload's answer; `Err` only when it could not be asked.
    fn upstream_has(&self, hash: &str, authorization: &str) -> Result<Result<bool, u16>> {
        if let Some(&has) = self.upstream_holds.lock().unwrap().get(hash) {
            return Ok(Ok(has));
        }
        // Never with both locks held at once, which `upload` takes the other
        // way round.
        let queued: Vec<String> = self.queued_blobs.lock().unwrap().keys().cloned().collect();
        let mut asked = vec![hash.to_owned()];
        {
            let known = self.upstream_holds.lock().unwrap();
            asked.extend(
                queued
                    .into_iter()
                    .filter(|queued| queued != hash && !known.contains_key(queued))
                    .take(MISSING_BATCH - 1),
            );
        }
        self.stats.upstream_requests.fetch_add(1, Ordering::Relaxed);
        let mut response = self
            .agent
            .post(format!("{}/v4/blobs/missing", self.upstream))
            .header("authorization", authorization)
            .header("content-type", "application/json")
            .send(serde_json::json!({ "blobs": asked }).to_string())?;
        if response.status().as_u16() != 200 {
            self.stats.upstream_requests.fetch_add(1, Ordering::Relaxed);
            let held = self
                .agent
                .head(format!("{}/v4/blobs/{hash}", self.upstream))
                .header("authorization", authorization)
                .call()?;
            return Ok(match held.status().as_u16() {
                200 => Ok(true),
                404 => Ok(false),
                status => Err(status),
            });
        }
        let answer: serde_json::Value =
            serde_json::from_str(&response.body_mut().read_to_string()?)?;
        let missing: HashSet<&str> = answer["missing"]
            .as_array()
            .context("no missing blobs in the upstream's answer")?
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect();
        let mut known = self.upstream_holds.lock().unwrap();
        for blob in &asked {
            known.insert(blob.clone(), !missing.contains(blob.as_str()));
        }
        Ok(Ok(!missing.contains(hash)))
    }

    /// The pack the upstream would answer for the entry or warm record queued
    /// at `path`, built from its upload: the table as one frame, then each
    /// blob the upload carried as the frame it came in. The rest are apart,
    /// as the proxy holds them.
    fn queued_pack(&self, path: &str) -> Option<Vec<u8>> {
        let id = *self.queued_records.lock().unwrap().get(path)?;
        let body = fs::read(self.body_path(id)).ok()?;
        pack_of_upload(&body)
    }

    fn record_file(&self, path: &str) -> PathBuf {
        self.records
            .join(blake3::hash(path.as_bytes()).to_hex().as_str())
    }

    /// Moves `body`, a record the upstream took, to where the proxy keeps the
    /// record at `path`, in place of an older one.
    fn keep_record(&self, path: &str, body: &Path) {
        let destination = self.record_file(path);
        {
            // Not while eviction counts what the proxy holds, which would
            // store its total over this change.
            let _evicting = self.evicting.lock().unwrap();
            let previous = fs::metadata(&destination).map_or(0, |metadata| metadata.len());
            let Ok(length) = fs::metadata(body).map(|metadata| metadata.len()) else {
                return;
            };
            if fs::rename(body, &destination).is_err() {
                return;
            }
            // Used now, as far as eviction goes, however long it was queued.
            let _ = File::options()
                .write(true)
                .open(&destination)
                .and_then(|file| file.set_modified(SystemTime::now()));
            self.size.fetch_add(length, Ordering::Relaxed);
            self.size.fetch_sub(previous, Ordering::Relaxed);
        }
        if self
            .max_size
            .is_some_and(|max| self.size.load(Ordering::Relaxed) > max)
        {
            self.evict();
        }
    }

    /// Removes the record kept for `path`, if any.
    fn forget_record(&self, path: &str) {
        let _evicting = self.evicting.lock().unwrap();
        let file = self.record_file(path);
        if let Ok(metadata) = fs::metadata(&file)
            && fs::remove_file(&file).is_ok()
        {
            self.size.fetch_sub(metadata.len(), Ordering::Relaxed);
        }
    }

    /// The pack of the warm record at `path` the proxy keeps, as the upstream
    /// would send it. Reading it counts as a use, which eviction goes by.
    fn kept_pack(&self, path: &str) -> Option<Vec<u8>> {
        let file = self.record_file(path);
        let body = fs::read(&file).ok()?;
        let _ = File::options()
            .write(true)
            .open(&file)
            .and_then(|file| file.set_modified(SystemTime::now()));
        pack_of_upload(&body)
    }

    /// Keeps `body`, an upload of the record at `path` the upstream took.
    fn keep_body(&self, path: &str, body: &[u8]) {
        if let Ok(mut file) = tempfile::NamedTempFile::new_in(&self.scratch)
            && file.write_all(body).is_ok()
        {
            self.keep_record(path, file.path());
        }
    }

    /// The whole index and its cursor: the copy, brought up to date with
    /// what changed upstream since, so that an entry committed before it was
    /// asked for is listed; or read whole again once it is old enough that
    /// keys the upstream dropped since matter. Listings asked for while one
    /// is brought up to date wait and share it, or its failure. Asked as
    /// `authorization`.
    fn index_listing(&self, authorization: &str) -> Result<(Arc<str>, String)> {
        let asked = Instant::now();
        let mut failed = self.catching_up.lock().unwrap();
        if failed.is_some_and(|began| began >= asked) {
            bail!("bringing the index up to date failed meanwhile");
        }
        let current = |copy: &Option<IndexCopy>| {
            copy.as_ref()
                .filter(|index| index.caught_up >= asked)
                .map(|index| (index.text.clone(), index.cursor.clone()))
        };
        if let Some(listing) = current(&self.index.lock().unwrap()) {
            return Ok(listing);
        }
        let started = Instant::now();
        let held = self.index.lock().unwrap().as_ref().and_then(|index| {
            (started.duration_since(index.read_whole) <= INDEX_REREAD).then(|| index.cursor.clone())
        });
        let (text, cursor) = match self.read_index(authorization, held.as_deref()) {
            Ok(read) => read,
            Err(error) => {
                *failed = Some(started);
                return Err(error);
            }
        };
        let mut copy = self.index.lock().unwrap();
        match (held, copy.as_mut()) {
            (Some(_), Some(index)) => index.merge(&text, cursor, started),
            _ => {
                let mut index = IndexCopy {
                    keys: BTreeMap::new(),
                    runs: String::new(),
                    cursor: String::new(),
                    text: Arc::from(""),
                    read_whole: started,
                    caught_up: started,
                };
                index.merge(&text, cursor, started);
                *copy = Some(index);
            }
        }
        Ok(current(&copy).expect("the index was brought up to date"))
    }

    /// The upstream's index, whole or since `cursor`, with its cursor.
    fn read_index(&self, authorization: &str, cursor: Option<&str>) -> Result<(String, String)> {
        self.stats.upstream_requests.fetch_add(1, Ordering::Relaxed);
        let target = match cursor {
            Some(cursor) => format!("{}/v4/index?since={cursor}", self.upstream),
            None => format!("{}/v4/index", self.upstream),
        };
        let mut response = self
            .agent
            .get(target)
            .header("authorization", authorization)
            .config()
            .timeout_global(Some(INDEX_TIMEOUT))
            .build()
            .call()?;
        if response.status().as_u16() != 200 {
            bail!("the upstream answered {} for its index", response.status());
        }
        let cursor = response
            .headers()
            .get("qk-index-cursor")
            .and_then(|value| value.to_str().ok())
            .context("the upstream's index has no cursor")?
            .to_owned();
        let text = response
            .body_mut()
            .with_config()
            .limit(LARGEST_INDEX)
            .read_to_string()?;
        self.stats
            .upstream_bytes
            .fetch_add(text.len() as u64, Ordering::Relaxed);
        Ok((text, cursor))
    }

    fn put(
        &self,
        target: &str,
        authorization: &str,
        content_type: Option<&str>,
        body: File,
    ) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
        self.stats.upstream_requests.fetch_add(1, Ordering::Relaxed);
        let mut request = self
            .agent
            .put(format!("{}{target}", self.upstream))
            .header("authorization", authorization);
        if let Some(content_type) = content_type {
            request = request.header("content-type", content_type);
        }
        request.send(body)
    }
}

/// What a status the upstream answered an upload with comes to.
/// The upstream's index as the proxy last read it: each key with its time,
/// and the runs, as the upstream lists them.
struct IndexCopy {
    keys: BTreeMap<String, String>,
    runs: String,
    cursor: String,
    /// The whole listing, as of `cursor`.
    text: Arc<str>,
    /// When the reads it was last read whole and last brought up to date
    /// with began.
    read_whole: Instant,
    caught_up: Instant,
}

impl IndexCopy {
    /// Takes in `text`, a listing of what changed, as of `cursor`: a key's
    /// time replaces the one held, and the runs, which every listing names
    /// whole, replace those held.
    fn merge(&mut self, text: &str, cursor: String, read: Instant) {
        self.caught_up = read;
        let mut runs = String::new();
        for line in text.lines() {
            if line.starts_with("run ") {
                runs.push_str(line);
                runs.push('\n');
            } else if let Some((key, time)) = line.split_once(' ') {
                self.keys.insert(key.to_owned(), time.to_owned());
            }
        }
        self.runs = runs;
        self.cursor = cursor;
        self.text = Arc::from(self.listing());
    }

    /// The whole listing, keys in order and then the runs.
    fn listing(&self) -> String {
        let mut text = String::with_capacity(self.keys.len() * 80 + self.runs.len());
        for (key, time) in &self.keys {
            text.push_str(key);
            text.push(' ');
            text.push_str(time);
            text.push('\n');
        }
        text.push_str(&self.runs);
        text
    }
}

/// Whether an upload to `target` is a bundle of blobs.
fn is_bundle(target: &str) -> bool {
    record_path(target).starts_with("/v4/bundles/")
}

fn sent(status: u16) -> Sent {
    match status {
        200..=299 => Sent::Done(status),
        429 | 500.. => Sent::Later(format!("the upstream answered {status}")),
        _ => Sent::Refused(format!("the upstream answered {status}")),
    }
}

/// The blobs the bundle uploaded to `path` carries, by hash, when it is one a
/// store server takes, as `tools/store-server/PROTOCOL.md` describes: within
/// the largest upload body, named by the SHA-256 of its header, its record
/// empty, and every blob it lists, sorted and once each, carried inline as a
/// zstd frame.
fn bundle_blobs<'a>(path: &str, body: &'a [u8]) -> Option<Vec<(String, &'a [u8])>> {
    use sha2::Digest;
    if body.len() > LARGEST_BUNDLE {
        return None;
    }
    let length = u32::from_le_bytes(body.get(..4)?.try_into().ok()?) as usize;
    let header = body.get(4..4 + length)?;
    let named: String = sha2::Sha256::digest(header)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if path.strip_prefix("/v4/bundles/") != Some(named.as_str()) {
        return None;
    }
    let parsed: serde_json::Value = serde_json::from_slice(header).ok()?;
    if parsed["record"].as_u64()? != 0 {
        return None;
    }
    let listed = parsed["blobs"].as_array()?;
    let mut at = 4 + length;
    let mut blobs: Vec<(String, &[u8])> = Vec::new();
    for blob in listed {
        let hash = blob[0].as_str()?;
        let valid = hash.len() == 64
            && hash
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'));
        let sorted = blobs.last().is_none_or(|(last, _)| last.as_str() < hash);
        blob[1].as_u64()?;
        let size = usize::try_from(blob[2].as_u64()?).ok()?;
        let frame = body.get(at..at.checked_add(size)?)?;
        if !valid || !sorted || !frame.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
            return None;
        }
        blobs.push((hash.to_owned(), frame));
        at += size;
    }
    (!blobs.is_empty() && at == body.len()).then_some(blobs)
}

/// The blobs an upload body carries inline, by hash: after the header and the
/// record, each blob with a compressed length is a zstd frame of that length,
/// in the header's order.
fn inline_blobs(body: &[u8]) -> Option<Vec<(String, &[u8])>> {
    let length = u32::from_le_bytes(body.get(..4)?.try_into().ok()?) as usize;
    let header: serde_json::Value = serde_json::from_slice(body.get(4..4 + length)?).ok()?;
    let mut at = 4 + length + usize::try_from(header["record"].as_u64()?).ok()?;
    let mut blobs = Vec::new();
    for blob in header["blobs"].as_array()? {
        let Some(size) = blob[2].as_u64() else {
            continue;
        };
        let size = usize::try_from(size).ok()?;
        let hash = blob[0].as_str()?.to_owned();
        blobs.push((hash, body.get(at..at.checked_add(size)?)?));
        at += size;
    }
    Some(blobs)
}

/// The distinct hashes a `POST blobs/fetch` body asks for, or `None` for a
/// body that does not parse or asks for more than the protocol allows.
fn fetch_query(body: &[u8]) -> Option<Vec<String>> {
    let parsed: serde_json::Value = serde_json::from_slice(body).ok()?;
    let listed = parsed.get("blobs")?.as_array()?;
    if listed.len() > FETCH_QUERY {
        return None;
    }
    let mut seen = HashSet::new();
    let mut asked = Vec::new();
    for hash in listed {
        let hash = hash.as_str()?;
        hash_bytes(hash)?;
        if seen.insert(hash) {
            asked.push(hash.to_owned());
        }
    }
    Some(asked)
}

/// The 32 bytes a blob's name, 64 lowercase hex digits, stands for.
fn hash_bytes(hash: &str) -> Option<[u8; 32]> {
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return None;
    }
    let mut bytes = [0; 32];
    for (at, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hash[at * 2..at * 2 + 2], 16).ok()?;
    }
    Some(bytes)
}

/// A request target without its query.
fn record_path(target: &str) -> &str {
    target.split('?').next().unwrap_or("")
}

/// The pack a store server sends for the upload body `body`: what precedes
/// the inline blobs as one frame, then each inline blob's frame unchanged.
/// Its layout is in `tools/store-server/PROTOCOL.md`.
fn pack_of_upload(body: &[u8]) -> Option<Vec<u8>> {
    let length = u32::from_le_bytes(body.get(..4)?.try_into().ok()?) as usize;
    let header: serde_json::Value = serde_json::from_slice(body.get(4..4 + length)?).ok()?;
    let record_length = usize::try_from(header["record"].as_u64()?).ok()?;
    let record = body.get(4 + length..4 + length + record_length)?;
    let blobs = header["blobs"].as_array()?;
    let mut table = b"qkp4".to_vec();
    table.extend((record.len() as u64).to_le_bytes());
    table.extend(record);
    table.extend(u32::try_from(blobs.len()).ok()?.to_le_bytes());
    for blob in blobs {
        table.push(u8::from(blob[2].as_u64().is_some()));
        table.extend(blob[1].as_u64()?.to_le_bytes());
    }
    let mut pack = zstd::bulk::compress(&table, 3).ok()?;
    pack.extend(body.get(4 + length + record_length..)?);
    Some(pack)
}

/// The blobs an upload body cites without carrying them, those stored apart:
/// the header's blobs without a compressed length.
fn apart_blobs(body: &[u8]) -> Option<Vec<String>> {
    let length = u32::from_le_bytes(body.get(..4)?.try_into().ok()?) as usize;
    let header: serde_json::Value = serde_json::from_slice(body.get(4..4 + length)?).ok()?;
    Some(
        header["blobs"]
            .as_array()?
            .iter()
            .filter(|blob| blob[2].as_u64().is_none())
            .filter_map(|blob| blob[0].as_str().map(str::to_owned))
            .collect(),
    )
}

/// The status line and headers; a body of unknown length is sent in chunks.
fn head(
    writer: &mut impl Write,
    status: u16,
    headers: &[(&str, &str)],
    length: Option<u64>,
) -> io::Result<()> {
    write!(writer, "HTTP/1.1 {status} {}\r\n", reason(status))?;
    for (name, value) in headers {
        write!(writer, "{name}: {value}\r\n")?;
    }
    match length {
        Some(length) => write!(writer, "content-length: {length}\r\n\r\n"),
        None => write!(writer, "transfer-encoding: chunked\r\n\r\n"),
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        413 => "Payload Too Large",
        422 => "Unprocessable Content",
        429 => "Too Many Requests",
        503 => "Service Unavailable",
        _ => "",
    }
}

fn bytes(count: u64) -> String {
    match count {
        count if count >= 1 << 30 => format!("{:.1}GB", count as f64 / (1u64 << 30) as f64),
        count if count >= 1 << 20 => format!("{:.1}MB", count as f64 / (1u64 << 20) as f64),
        count if count >= 1 << 10 => format!("{:.1}KB", count as f64 / 1024.0),
        count => format!("{count}B"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framing_two_readers_could_take_differently_is_refused() {
        let refused = |request: &str| {
            format!(
                "{:#}",
                read_request(&mut io::Cursor::new(request.as_bytes().to_vec())).unwrap_err()
            )
        };
        assert!(
            refused(
                "PUT /v4/blobs/x HTTP/1.1\r\nContent-Length: 4\r\nContent-Length: 5\r\n\r\nabcd"
            )
            .contains("conflicting content-length")
        );
        assert!(
            refused("PUT /v4/blobs/x HTTP/1.1\r\nContent-Length: 4\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n")
                .contains("both content-length and transfer-encoding")
        );
        assert!(
            refused(
                "PUT /v4/blobs/x HTTP/1.1\r\nTransfer-Encoding: gzip, chunked\r\n\r\n0\r\n\r\n"
            )
            .contains("unsupported transfer-encoding")
        );
        assert!(
            refused("PUT /v4/blobs/x HTTP/1.1\r\nTransfer-Encoding: chunked\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n")
                .contains("more than one transfer-encoding")
        );
        // A name with whitespace before its colon, which a lenient reader
        // could take as the length, and a line that is no header at all.
        assert!(
            refused("PUT /v4/blobs/x HTTP/1.1\r\nContent-Length : 4\r\n\r\nabcd")
                .contains("malformed header name")
        );
        assert!(
            refused("PUT /v4/blobs/x HTTP/1.1\r\nContent-Length: 4\r\n folded\r\n\r\nabcd")
                .contains("malformed header line")
        );
        // A repeated length that agrees is no conflict.
        let agreed = read_request(&mut io::Cursor::new(
            b"PUT /v4/blobs/x HTTP/1.1\r\nContent-Length: 4\r\ncontent-length: 4\r\n\r\nabcd"
                .to_vec(),
        ))
        .unwrap()
        .unwrap();
        assert_eq!(agreed.body, b"abcd");
    }

    #[test]
    fn only_a_path_may_be_requested() {
        let error = read_request(&mut io::Cursor::new(
            b"GET @internal-host:8080/x HTTP/1.1\r\n\r\n".to_vec(),
        ))
        .unwrap_err();
        assert!(format!("{error:#}").contains("unsupported request target"));
    }

    #[test]
    fn sizes_past_the_limits_are_refused_before_they_are_read() {
        let refused = |request: Vec<u8>| {
            format!(
                "{:#}",
                read_request(&mut io::Cursor::new(request)).unwrap_err()
            )
        };
        let huge = format!(
            "PUT /v4/blobs/x HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY + 1
        );
        assert!(refused(huge.into_bytes()).contains("larger than the proxy takes"));
        let chunk = format!(
            "PUT /v4/blobs/x HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n",
            MAX_BODY + 1
        );
        assert!(refused(chunk.into_bytes()).contains("larger than the proxy takes"));
        let mut long = b"GET /".to_vec();
        long.extend(vec![b'a'; MAX_LINE as usize + 10]);
        long.extend(b" HTTP/1.1\r\n\r\n");
        assert!(refused(long).contains("too long"));
        assert!(
            refused(b"PUT /v4/blobs/x HTTP/1.1\r\nContent-Length: 10\r\n\r\nshort".to_vec())
                .contains("ends early")
        );
    }

    #[test]
    fn chunked_bodies_are_read_whole() {
        let mut reader = io::Cursor::new(b"4\r\nwiki\r\n5;x=y\r\npedia\r\n0\r\n\r\n".to_vec());
        assert_eq!(read_chunked(&mut reader).unwrap(), b"wikipedia");
    }

    #[test]
    fn a_request_without_a_body_ends_at_its_headers() {
        let mut reader = io::Cursor::new(
            b"HEAD /v4/blobs/x HTTP/1.1\r\nAuthorization: Bearer t\r\n\r\nGET /v4/index HTTP/1.1\r\nConnection: close\r\n\r\n"
                .to_vec(),
        );
        let first = read_request(&mut reader).unwrap().unwrap();
        assert_eq!(
            (first.method.as_str(), first.target.as_str()),
            ("HEAD", "/v4/blobs/x")
        );
        assert_eq!(first.authorization.as_deref(), Some("Bearer t"));
        assert!(!first.close);
        let second = read_request(&mut reader).unwrap().unwrap();
        assert!(second.close);
        assert!(read_request(&mut reader).unwrap().is_none());
    }

    #[test]
    fn a_record_waits_only_for_the_jobs_of_its_blobs_queued_before_it() {
        let directory = tempfile::tempdir().unwrap();
        let proxy = Proxy::open(&Options {
            upstream: "http://127.0.0.1:9".into(),
            directory: directory.path().to_owned(),
            listen: String::new(),
            max_size: None,
        })
        .unwrap();
        let blob = "b".repeat(64);
        let request = |target: String, body: Vec<u8>| Request {
            method: "PUT".into(),
            target,
            authorization: Some("Bearer token".into()),
            content_type: None,
            body,
            close: false,
        };
        // A record citing the blob stored apart, as an upload body.
        let record = b"{}";
        let header = serde_json::json!({ "record": record.len(), "blobs": [[blob, 1, null]] });
        let header = serde_json::to_vec(&header).unwrap();
        let mut body = (header.len() as u32).to_le_bytes().to_vec();
        body.extend(&header);
        body.extend(record);
        let waits = |id: u64| {
            let (done, finished) = std::sync::mpsc::channel();
            let proxy = proxy.clone();
            std::thread::spawn(move || {
                proxy.await_blobs(id);
                let _ = done.send(());
            });
            finished
        };

        // The same blob uploaded again after the record is not waited for.
        proxy
            .enqueue(&request("/v4/entries/first".into(), body.clone()), None)
            .unwrap();
        proxy
            .enqueue(
                &request(format!("/v4/blobs/{blob}"), Vec::new()),
                Some(&blob),
            )
            .unwrap();
        assert!(waits(0).recv_timeout(Duration::from_secs(5)).is_ok());

        // A record after the blob's job waits until that job is done.
        proxy
            .enqueue(&request("/v4/entries/second".into(), body), None)
            .unwrap();
        let finished = waits(2);
        assert!(finished.recv_timeout(Duration::from_millis(200)).is_err());
        proxy.queued_blobs.lock().unwrap().remove(&blob);
        proxy.blob_done.notify_all();
        assert!(finished.recv_timeout(Duration::from_secs(5)).is_ok());
    }

    #[test]
    fn only_bundles_a_store_server_takes_are_queued() {
        use sha2::Digest;
        let frame = zstd::stream::encode_all(&b"x"[..], 1).unwrap();
        // A bundle of `listed`, each carried as `frame` when its third field
        // is a length, and its path named by its header unless `path` says.
        let bundle = |record: u64, listed: serde_json::Value, path: Option<&str>| {
            let header =
                serde_json::to_vec(&serde_json::json!({"record": record, "blobs": listed}))
                    .unwrap();
            let mut body = (header.len() as u32).to_le_bytes().to_vec();
            body.extend(&header);
            body.extend(vec![b'r'; record as usize]);
            for blob in listed.as_array().unwrap() {
                if blob[2].is_u64() {
                    body.extend(&frame);
                }
            }
            let named: String = sha2::Sha256::digest(&header)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            let path = path.map_or(format!("/v4/bundles/{named}"), str::to_owned);
            (path, body)
        };
        let (a, b) = ("a".repeat(64), "b".repeat(64));
        let size = frame.len();
        let takes = |(path, body): (String, Vec<u8>)| bundle_blobs(&path, &body).is_some();
        assert!(takes(bundle(
            0,
            serde_json::json!([[a, 1, size], [b, 1, size]]),
            None
        )));
        // Named otherwise, carrying a record, listing a blob it does not
        // carry, unsorted, or carrying nothing.
        let other = format!("/v4/bundles/{}", "c".repeat(64));
        assert!(!takes(bundle(
            0,
            serde_json::json!([[a, 1, size]]),
            Some(&other)
        )));
        assert!(!takes(bundle(2, serde_json::json!([[a, 1, size]]), None)));
        assert!(!takes(bundle(
            0,
            serde_json::json!([[a, 1, size], [b, 1, null]]),
            None
        )));
        assert!(!takes(bundle(
            0,
            serde_json::json!([[b, 1, size], [a, 1, size]]),
            None
        )));
        assert!(!takes(bundle(0, serde_json::json!([]), None)));
    }

    #[test]
    fn a_record_waits_for_a_bundle_queued_before_it_carrying_its_blobs() {
        let directory = tempfile::tempdir().unwrap();
        let proxy = Proxy::open(&Options {
            upstream: "http://127.0.0.1:9".into(),
            directory: directory.path().to_owned(),
            listen: String::new(),
            max_size: None,
        })
        .unwrap();
        let blob = "b".repeat(64);
        let request = |target: String, body: Vec<u8>| Request {
            method: "PUT".into(),
            target,
            authorization: Some("Bearer token".into()),
            content_type: None,
            body,
            close: false,
        };
        let body = |record: &[u8], frame: Option<&[u8]>| {
            let header = serde_json::json!({
                "record": record.len(),
                "blobs": [[blob, 1, frame.map(<[u8]>::len)]],
            });
            let header = serde_json::to_vec(&header).unwrap();
            let mut body = (header.len() as u32).to_le_bytes().to_vec();
            body.extend(&header);
            body.extend(record);
            body.extend(frame.unwrap_or_default());
            body
        };
        let frame = zstd::stream::encode_all(&b"x"[..], 1).unwrap();
        proxy
            .enqueue(
                &request(
                    format!("/v4/bundles/{}", "c".repeat(64)),
                    body(b"", Some(&frame)),
                ),
                None,
            )
            .unwrap();
        // A record citing the bundled blob stored apart waits for the bundle.
        proxy
            .enqueue(&request("/v4/entries/key".into(), body(b"{}", None)), None)
            .unwrap();
        let (done, finished) = std::sync::mpsc::channel();
        let waiting = proxy.clone();
        std::thread::spawn(move || {
            waiting.await_blobs(1);
            let _ = done.send(());
        });
        assert!(finished.recv_timeout(Duration::from_millis(200)).is_err());
        proxy.queued_blobs.lock().unwrap().remove(&blob);
        proxy.blob_done.notify_all();
        assert!(finished.recv_timeout(Duration::from_secs(5)).is_ok());
    }

    #[test]
    fn a_blob_the_upstream_refuses_to_answer_about_is_given_up() {
        // An upstream that refuses every request, as one whose token was
        // revoked after the proxy queued its uploads.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let upstream = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut writer = stream;
                    while let Ok(Some(_)) = read_request(&mut reader) {
                        let refused = "HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\n\r\n";
                        if writer.write_all(refused.as_bytes()).is_err() {
                            return;
                        }
                    }
                });
            }
        });
        let directory = tempfile::tempdir().unwrap();
        let proxy = Proxy::open(&Options {
            upstream,
            directory: directory.path().to_owned(),
            listen: String::new(),
            max_size: None,
        })
        .unwrap();
        let blob = "c".repeat(64);
        match proxy.send_blob(&blob, "Bearer revoked", true) {
            Sent::Refused(why) => assert!(why.contains("401"), "{why}"),
            Sent::Done(status) => panic!("sent: {status}"),
            Sent::Missing => panic!("missing"),
            Sent::Later(why) => panic!("left to send again: {why}"),
        }
    }

    #[test]
    fn an_uploaded_blob_is_queued_without_eviction_taking_it_first() {
        let upload = |content: &[u8]| {
            let hash = blake3::hash(content).to_hex().to_string();
            let request = Request {
                method: "PUT".into(),
                target: format!("/v4/blobs/{hash}"),
                authorization: Some("Bearer token".into()),
                content_type: Some("application/zstd".into()),
                body: zstd::stream::encode_all(content, 3).unwrap(),
                close: false,
            };
            (hash, request)
        };
        let (first, first_request) = upload(b"the first upload, queued and so held");
        let (second, second_request) =
            upload(b"the second upload, which keeping goes past the limit");
        let directory = tempfile::tempdir().unwrap();
        // Room for the first and not both, so keeping the second evicts.
        let room = first_request.body.len() + second_request.body.len() - 1;
        let proxy = Proxy::open(&Options {
            upstream: "http://127.0.0.1:9".into(),
            directory: directory.path().to_owned(),
            listen: String::new(),
            max_size: Some(room as u64),
        })
        .unwrap();
        assert!(proxy.queue_blob(&first_request, &first).unwrap());
        assert!(proxy.queue_blob(&second_request, &second).unwrap());
        assert!(proxy.blobs.join(&first).is_file());
        assert!(
            proxy.blobs.join(&second).is_file(),
            "the queued blob was evicted"
        );
        assert_eq!(proxy.jobs.lock().unwrap().len(), 2);
    }

    #[test]
    fn a_listing_shares_the_failure_of_a_catch_up_begun_after_it_was_asked_for() {
        let directory = tempfile::tempdir().unwrap();
        let proxy = Proxy::open(&Options {
            upstream: "http://127.0.0.1:9".into(),
            directory: directory.path().to_owned(),
            listen: String::new(),
            max_size: None,
        })
        .unwrap();
        // One that failed after this listing was asked for is its answer,
        // without asking the upstream again.
        *proxy.catching_up.lock().unwrap() = Some(Instant::now() + Duration::from_secs(60));
        let shared = proxy.index_listing("Bearer token").unwrap_err();
        assert!(shared.to_string().contains("failed meanwhile"), "{shared}");
        // One that failed before it was asked for is not: it asks itself.
        *proxy.catching_up.lock().unwrap() = Some(Instant::now() - Duration::from_secs(60));
        let own = proxy.index_listing("Bearer token").unwrap_err();
        assert!(!own.to_string().contains("failed meanwhile"), "{own}");
        assert_eq!(proxy.stats.upstream_requests.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_copy_of_the_index_takes_in_what_changed() {
        let now = Instant::now();
        let mut index = IndexCopy {
            keys: BTreeMap::new(),
            runs: String::new(),
            cursor: String::new(),
            text: Arc::from(""),
            read_whole: now,
            caught_up: now,
        };
        index.merge("b 1\na 1\nrun r1 5 m1\n", "7".into(), now);
        // A key's newer time replaces its older one, a new key joins, and the
        // runs, which every listing names whole, replace those held.
        index.merge("b 9\nc 2\nrun r2 8 m2\n", "9".into(), now);
        assert_eq!(&*index.text, "a 1\nb 9\nc 2\nrun r2 8 m2\n");
        assert_eq!(index.cursor, "9");
    }

    #[test]
    fn kept_records_are_evicted_least_recently_used_first() {
        let directory = tempfile::tempdir().unwrap();
        // Room for two records of 100 bytes and not three.
        let proxy = Proxy::open(&Options {
            upstream: "http://127.0.0.1:9".into(),
            directory: directory.path().to_owned(),
            listen: String::new(),
            max_size: Some(250),
        })
        .unwrap();
        let keep = |path: &str, age: u64| {
            let body = directory.path().join("body");
            fs::write(&body, [0; 100]).unwrap();
            proxy.keep_record(path, &body);
            File::options()
                .write(true)
                .open(proxy.record_file(path))
                .unwrap()
                .set_modified(SystemTime::now() - Duration::from_secs(age))
                .unwrap();
        };
        keep("/v4/warm/a/first", 30);
        keep("/v4/warm/a/second", 20);
        // Read since, and so used more recently than the second.
        proxy.kept_pack("/v4/warm/a/first");
        keep("/v4/warm/a/third", 0);
        assert!(proxy.record_file("/v4/warm/a/first").is_file());
        assert!(!proxy.record_file("/v4/warm/a/second").exists());
        assert!(proxy.record_file("/v4/warm/a/third").is_file());
        assert_eq!(proxy.size.load(Ordering::Relaxed), 200);
        // And one the upstream took a newer upload of directly is forgotten.
        proxy.forget_record("/v4/warm/a/third");
        assert!(!proxy.record_file("/v4/warm/a/third").exists());
        assert_eq!(proxy.size.load(Ordering::Relaxed), 100);
    }

    #[test]
    fn records_of_one_path_are_sent_one_at_a_time_and_the_newest_wins() {
        let directory = tempfile::tempdir().unwrap();
        let proxy = Proxy::open(&Options {
            upstream: "http://127.0.0.1:9".into(),
            directory: directory.path().to_owned(),
            listen: String::new(),
            max_size: None,
        })
        .unwrap();
        let path = "/v4/warm/a/b";
        let request = |body: &[u8]| Request {
            method: "PUT".into(),
            target: path.into(),
            authorization: Some("Bearer token".into()),
            content_type: None,
            body: body.to_vec(),
            close: false,
        };
        proxy.enqueue(&request(b"older"), None).unwrap();
        proxy.enqueue(&request(b"newer"), None).unwrap();
        // The older upload gives way to the newer, which takes the turn.
        assert!(proxy.record_turn(path, 0).is_none());
        // And still once the newer was sent and so left the queue.
        let newest = proxy.queued_records.lock().unwrap().remove(path).unwrap();
        assert!(proxy.record_turn(path, 0).is_none());
        proxy
            .queued_records
            .lock()
            .unwrap()
            .insert(path.to_owned(), newest);
        let turn = proxy.record_turn(path, 1).unwrap();
        // Another send of the path waits for the turn to end.
        let (taken, waiting) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let second = proxy.record_turn(path, 1);
                taken.send(second.is_some()).unwrap();
            });
            assert!(waiting.recv_timeout(Duration::from_millis(200)).is_err());
            drop(turn);
            assert!(waiting.recv_timeout(Duration::from_secs(5)).unwrap());
        });
    }

    #[test]
    fn a_proxy_started_again_resumes_the_uploads_it_queued_in_order() {
        let directory = tempfile::tempdir().unwrap();
        let options = Options {
            upstream: "http://127.0.0.1:9".into(),
            directory: directory.path().to_owned(),
            listen: String::new(),
            max_size: None,
        };
        let upload = |target: &str, body: &[u8]| Request {
            method: "PUT".into(),
            target: target.into(),
            authorization: Some("Bearer token".into()),
            content_type: Some("application/x-qk-upload".into()),
            body: body.to_vec(),
            close: false,
        };
        let blob = "a".repeat(64);
        let first = Proxy::open(&options).unwrap();
        first
            .enqueue(&upload(&format!("/v4/blobs/{blob}"), b""), Some(&blob))
            .unwrap();
        // Queued once while it waits to be sent.
        first
            .enqueue(&upload(&format!("/v4/blobs/{blob}"), b""), Some(&blob))
            .unwrap();
        first
            .enqueue(&upload("/v4/entries/key", b"record"), None)
            .unwrap();
        drop(first);
        // A file a stop cut short of being put in place.
        fs::write(directory.path().join("queue/.tmp123"), b"").unwrap();

        let second = Proxy::open(&options).unwrap();
        assert_eq!(second.resume_queue().unwrap(), 2);
        assert_eq!(*second.jobs.lock().unwrap(), [0, 1]);
        assert_eq!(second.queued_blobs.lock().unwrap()[&blob].len(), 1);
        assert_eq!(fs::read(second.body_path(1)).unwrap(), b"record");
        assert!(!directory.path().join("queue/.tmp123").exists());
        // Jobs queued after continue the order.
        second
            .enqueue(&upload("/v4/entries/other", b"record"), None)
            .unwrap();
        assert_eq!(*second.jobs.lock().unwrap(), [0, 1, 2]);
    }
}
