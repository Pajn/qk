//! `qk store proxy`: a store server in front of another, for machines on one
//! network. Blobs never change once stored, so the proxy keeps those it passes
//! on its own disk and serves them from there; every other request goes to the
//! upstream server as it came, so writes, records that change and the index
//! stay the upstream's. Runners point `QK_STORE_SERVER_URL` at the proxy and
//! keep their tokens, which the upstream checks.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};

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
    max_size: Option<u64>,
    /// Tokens the upstream accepted, which may read blobs held here.
    trusted: Mutex<HashSet<String>>,
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
}

pub fn serve(options: Options) -> Result<()> {
    let blobs = options.directory.join("blobs");
    let scratch = options.directory.join("tmp");
    fs::create_dir_all(&blobs).with_context(|| format!("cannot create {}", blobs.display()))?;
    fs::create_dir_all(&scratch)?;
    // Partial downloads of an earlier run are no blob.
    for entry in fs::read_dir(&scratch)?.flatten() {
        let _ = fs::remove_file(entry.path());
    }
    let size = fs::read_dir(&blobs)?
        .flatten()
        .filter_map(|entry| entry.metadata().ok())
        .map(|metadata| metadata.len())
        .sum();
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .timeout_connect(Some(Duration::from_secs(10)))
        .build()
        .into();
    let proxy = Arc::new(Proxy {
        agent,
        upstream: options.upstream.trim_end_matches('/').to_owned(),
        blobs,
        scratch,
        max_size: options.max_size,
        trusted: Mutex::new(HashSet::new()),
        evicting: Mutex::new(()),
        size: AtomicU64::new(size),
        stats: Stats::default(),
    });
    let listener = TcpListener::bind(&options.listen)
        .with_context(|| format!("cannot listen on {}", options.listen))?;
    eprintln!(
        "qk: store proxy for {} listening on http://{}, holding {} in {}",
        proxy.upstream,
        listener.local_addr()?,
        bytes(size),
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
        let Some((name, value)) = header.split_once(':') else {
            continue;
        };
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
    let path = request.target.split('?').next().unwrap_or("");
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
    let keep = match (request.method.as_str(), blob) {
        ("GET", Some(hash)) => Some(hash),
        _ => None,
    };
    let (status, sent) = proxy.forward(request, writer, keep)?;
    // An upload the upstream took is kept too, for the runners that fetch it:
    // a blob on its own, or those a result or warm record carries inline.
    // The client has its answer first.
    if request.method == "PUT" && status == 204 {
        writer.flush()?;
        if let Some(hash) = blob {
            proxy.keep(hash, &request.body);
        } else if path.starts_with("/v4/entries/") || path.starts_with("/v4/warm/") {
            for (hash, frame) in inline_blobs(&request.body).unwrap_or_default() {
                proxy.keep(&hash, frame);
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

    /// Keeps a blob the proxy holds in memory.
    fn keep(&self, hash: &str, frame: &[u8]) {
        if self.held(hash) {
            return;
        }
        let Ok(mut file) = tempfile::NamedTempFile::new_in(&self.scratch) else {
            return;
        };
        if file.write_all(frame).is_ok() {
            self.store(hash, file);
        }
    }

    /// Moves a downloaded frame into place once it decodes to its hash: the
    /// proxy serves what it keeps without asking the upstream again.
    fn store(&self, hash: &str, file: tempfile::NamedTempFile) {
        // Kept meanwhile, as by another connection: counted once.
        if self.held(hash) {
            return;
        }
        let verified = File::open(file.path()).ok().is_some_and(|frame| {
            let mut hasher = blake3::Hasher::new();
            zstd::stream::read::Decoder::new(frame)
                .and_then(|mut decoder| io::copy(&mut decoder, &mut hasher))
                .is_ok()
                && hasher.finalize().to_hex().as_str() == hash
        });
        if !verified {
            return;
        }
        let length = file
            .as_file()
            .metadata()
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        if file.persist(self.blobs.join(hash)).is_ok() {
            self.stats.stored.fetch_add(1, Ordering::Relaxed);
            let size = self.size.fetch_add(length, Ordering::Relaxed) + length;
            if self.max_size.is_some_and(|max| size > max) {
                self.evict();
            }
        }
    }

    /// Removes the least recently used blobs until the proxy holds at most 90%
    /// of its limit.
    fn evict(&self) {
        let Some(max) = self.max_size else { return };
        let Ok(_guard) = self.evicting.try_lock() else {
            return;
        };
        let Ok(entries) = fs::read_dir(&self.blobs) else {
            return;
        };
        let mut blobs: Vec<(SystemTime, u64, PathBuf)> = entries
            .flatten()
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
            if fs::remove_file(&path).is_ok() {
                size -= length;
                self.stats.evicted.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.size.store(size, Ordering::Relaxed);
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
            "size": load(&self.size),
        })
        .to_string()
    }
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
        blobs.push((hash, body.get(at..at + size)?));
        at += size;
    }
    Some(blobs)
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
}
