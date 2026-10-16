//! An in-process stand-in for a qk store server, following
//! `tools/store-server/PROTOCOL.md` over plain HTTP, with records and blobs in
//! memory. Enough to exercise qk's store server backend.

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const READ_TOKEN: &str = "read-token";
pub const WRITE_TOKEN: &str = "write-token";

#[derive(Default)]
pub struct State {
    /// Upload bodies by `entries/<key>` or `warm/<a>/<b>`.
    pub records: BTreeMap<String, Vec<u8>>,
    /// Blobs stored apart, each one zstd frame, by hash.
    pub blobs: BTreeMap<String, Vec<u8>>,
    sessions: HashMap<String, Vec<u8>>,
    /// Each key's time and the sequence number of its last change.
    keys: BTreeMap<String, (u64, u64)>,
    runs: BTreeMap<String, String>,
    sequence: u64,
    /// Methods and paths, with the size of each response body, in order.
    pub requests: Vec<(String, usize)>,
    /// Take every blob as held, as a filter's false positive would.
    pub omit_all: bool,
    /// Answer the next entry commit with 422 for its blobs stored apart,
    /// forgetting them, as a store that lost them would.
    pub fail_commit_once: bool,
    pub latency: Duration,
}

pub struct FakeStoreServer {
    pub url: String,
    pub state: Arc<Mutex<State>>,
}

impl FakeStoreServer {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(State::default()));
        let shared = state.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let state = shared.clone();
                std::thread::spawn(move || {
                    let _ = serve(stream, &state);
                });
            }
        });
        Self { url, state }
    }

    /// The requests whose path starts with `/v4/<prefix>`.
    #[cfg(unix)]
    pub fn requests(&self, prefix: &str) -> Vec<(String, usize)> {
        let prefix = format!("/v4/{prefix}");
        self.state
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|(request, _)| {
                request
                    .split_once(' ')
                    .is_some_and(|(_, path)| path.starts_with(&prefix))
            })
            .cloned()
            .collect()
    }
}

struct Upload {
    record: Vec<u8>,
    /// Each blob's hash, raw length and, when carried inline, its frame.
    blobs: Vec<(String, u64, Option<Vec<u8>>)>,
}

fn parse_upload(body: &[u8]) -> Option<Upload> {
    let length = u32::from_le_bytes(body.get(..4)?.try_into().ok()?) as usize;
    let header: serde_json::Value = serde_json::from_slice(body.get(4..4 + length)?).ok()?;
    let mut at = 4 + length;
    let record_length = header["record"].as_u64()? as usize;
    let record = body.get(at..at + record_length)?.to_vec();
    at += record_length;
    let mut blobs = Vec::new();
    for blob in header["blobs"].as_array()? {
        let hash = blob[0].as_str()?.to_owned();
        let raw = blob[1].as_u64()?;
        let frame = match blob[2].as_u64() {
            Some(size) => {
                let frame = body.get(at..at + size as usize)?.to_vec();
                at += size as usize;
                Some(frame)
            }
            None => None,
        };
        blobs.push((hash, raw, frame));
    }
    (at == body.len()).then_some(Upload { record, blobs })
}

/// Whether the Bloom filter `bloom`, in the protocol's layout, holds `hash`.
fn holds(bloom: &[u8], hash: &str) -> bool {
    let k = u32::from_le_bytes(bloom[4..8].try_into().unwrap()) as u64;
    let m = u64::from_le_bytes(bloom[8..16].try_into().unwrap());
    let bits = &bloom[16..];
    let bytes: Vec<u8> = (0..32)
        .map(|index| u8::from_str_radix(&hash[index * 2..index * 2 + 2], 16).unwrap_or(0))
        .collect();
    let h1 = u64::from_le_bytes(bytes[..8].try_into().unwrap());
    let h2 = u64::from_le_bytes(bytes[8..16].try_into().unwrap()) | 1;
    (0..k).all(|i| {
        let bit = h1.wrapping_add(i.wrapping_mul(h2)) % m;
        bits[(bit >> 3) as usize] & (1 << (bit & 7)) != 0
    })
}

/// The pack for a stored upload, leaving out what `held` says the client has.
fn compose(state: &State, upload: &Upload, held: impl Fn(&str) -> bool) -> Vec<u8> {
    let mut header = b"qkp4".to_vec();
    header.extend_from_slice(&(upload.record.len() as u64).to_le_bytes());
    header.extend_from_slice(&upload.record);
    header.extend_from_slice(&(upload.blobs.len() as u32).to_le_bytes());
    let mut bodies = Vec::new();
    for (hash, raw, frame) in &upload.blobs {
        let frame = frame.as_ref().or_else(|| state.blobs.get(hash));
        let inline = frame.filter(|_| !held(hash));
        header.push(u8::from(inline.is_some()));
        header.extend_from_slice(&raw.to_le_bytes());
        if let Some(frame) = inline {
            bodies.extend_from_slice(frame);
        }
    }
    let mut pack = zstd::stream::encode_all(&header[..], 1).unwrap();
    pack.extend_from_slice(&bodies);
    pack
}

fn serve(stream: TcpStream, state: &Mutex<State>) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let target = parts.next().unwrap_or_default().to_owned();
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    let parameter = |name: &str| {
        query.split('&').find_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            (key == name).then(|| value.to_owned())
        })
    };
    let mut length = 0;
    let mut token = String::new();
    loop {
        let mut header = String::new();
        reader.read_line(&mut header)?;
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        let (name, value) = header.split_once(':').unwrap_or((header, ""));
        match name.to_ascii_lowercase().as_str() {
            "content-length" => length = value.trim().parse().unwrap_or(0),
            "authorization" => {
                token = value.trim().trim_start_matches("Bearer ").to_owned();
            }
            _ => {}
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    let latency = state.lock().unwrap().latency;
    std::thread::sleep(latency);
    let writes = method == "PUT" || (method == "POST" && path == "/v4/runs");
    let (status, headers, response) = {
        let mut state = state.lock().unwrap();
        let answer: (u16, Vec<(String, String)>, Vec<u8>) =
            if token != READ_TOKEN && token != WRITE_TOKEN {
                (
                    401,
                    Vec::new(),
                    br#"{"error":"unauthorized","message":"no"}"#.to_vec(),
                )
            } else if writes && token != WRITE_TOKEN {
                (
                    403,
                    Vec::new(),
                    br#"{"error":"read_only","message":"read only"}"#.to_vec(),
                )
            } else {
                handle(&mut state, &method, path, &parameter, body)
            };
        state
            .requests
            .push((format!("{method} {path}"), answer.2.len()));
        answer
    };
    let mut stream = stream;
    write!(
        stream,
        "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n",
        response.len()
    )?;
    for (name, value) in headers {
        write!(stream, "{name}: {value}\r\n")?;
    }
    write!(stream, "\r\n")?;
    if method != "HEAD" {
        stream.write_all(&response)?;
    }
    stream.flush()
}

type Answer = (u16, Vec<(String, String)>, Vec<u8>);

fn handle(
    state: &mut State,
    method: &str,
    path: &str,
    parameter: &dyn Fn(&str) -> Option<String>,
    body: Vec<u8>,
) -> Answer {
    let found = |bytes: Vec<u8>| (200, Vec::new(), bytes);
    let missing = || (404, Vec::new(), Vec::new());
    let done = || (204, Vec::new(), Vec::new());
    let Some(path) = path.strip_prefix("/v4/") else {
        return missing();
    };
    match (method, path) {
        ("POST", "sessions") => {
            let id = format!("{:032x}", state.sessions.len() + 1);
            state.sessions.insert(id.clone(), body);
            let answer = serde_json::json!({"session": id, "expires": u64::MAX});
            (201, Vec::new(), answer.to_string().into_bytes())
        }
        ("GET", "index") => {
            let since: u64 = parameter("since")
                .and_then(|since| since.parse().ok())
                .unwrap_or(0);
            let mut text = String::new();
            for (key, (time, sequence)) in &state.keys {
                if *sequence > since {
                    text.push_str(&format!("{key} {time}\n"));
                }
            }
            for line in state.runs.values() {
                text.push_str(line);
                text.push('\n');
            }
            let cursor = vec![("Qk-Index-Cursor".to_owned(), state.sequence.to_string())];
            (200, cursor, text.into_bytes())
        }
        ("POST", "runs") => {
            let text = String::from_utf8_lossy(&body).into_owned();
            for line in text.lines() {
                if let Some(run) = line.strip_prefix("run ") {
                    let id = run.split(' ').next().unwrap_or_default().to_owned();
                    state.runs.insert(id, line.to_owned());
                } else if let Some((key, time)) = line.split_once(' ')
                    && let Ok(time) = time.parse::<u64>()
                    && state.keys.contains_key(key)
                {
                    state.sequence += 1;
                    let sequence = state.sequence;
                    state.keys.insert(key.to_owned(), (time, sequence));
                }
            }
            done()
        }
        (method, path) if path.starts_with("blobs/") => {
            let hash = &path["blobs/".len()..];
            match method {
                "PUT" => {
                    state.blobs.insert(hash.to_owned(), body);
                    done()
                }
                "HEAD" => match state.blobs.contains_key(hash) {
                    true => found(Vec::new()),
                    false => missing(),
                },
                "GET" => {
                    if let Some(frame) = state.blobs.get(hash) {
                        return found(frame.clone());
                    }
                    let owner = parameter("entry")
                        .map(|key| format!("entries/{key}"))
                        .or_else(|| parameter("warm").map(|names| format!("warm/{names}")));
                    let inline = owner
                        .and_then(|owner| state.records.get(&owner))
                        .and_then(|stored| parse_upload(stored))
                        .and_then(|upload| {
                            upload
                                .blobs
                                .into_iter()
                                .find(|(blob, _, _)| blob == hash)
                                .and_then(|(_, _, frame)| frame)
                        });
                    inline.map_or_else(missing, found)
                }
                _ => (405, Vec::new(), Vec::new()),
            }
        }
        (method, path) if path.starts_with("entries/") || path.starts_with("warm/") => {
            let entry = path.starts_with("entries/");
            match method {
                "HEAD" => match state.records.contains_key(path) {
                    true => found(Vec::new()),
                    false => missing(),
                },
                "GET" => {
                    let Some(upload) = state.records.get(path).and_then(|body| parse_upload(body))
                    else {
                        return missing();
                    };
                    let bloom = parameter("session").and_then(|id| state.sessions.get(&id));
                    let omit_all = state.omit_all;
                    let pack = compose(state, &upload, |hash| {
                        omit_all || bloom.is_some_and(|bloom| holds(bloom, hash))
                    });
                    found(pack)
                }
                "PUT" => {
                    let Some(upload) = parse_upload(&body) else {
                        return (
                            400,
                            Vec::new(),
                            br#"{"error":"malformed","message":"no"}"#.to_vec(),
                        );
                    };
                    let apart: Vec<String> = upload
                        .blobs
                        .iter()
                        .filter(|(_, _, frame)| frame.is_none())
                        .map(|(hash, _, _)| hash.clone())
                        .collect();
                    if entry && state.fail_commit_once && !apart.is_empty() {
                        state.fail_commit_once = false;
                        for hash in &apart {
                            state.blobs.remove(hash);
                        }
                    }
                    let lacking: Vec<String> = apart
                        .into_iter()
                        .filter(|hash| !state.blobs.contains_key(hash))
                        .collect();
                    if !lacking.is_empty() {
                        let answer = serde_json::json!({
                            "error": "missing_blobs", "message": "missing", "blobs": lacking
                        });
                        return (422, Vec::new(), answer.to_string().into_bytes());
                    }
                    if entry && state.records.contains_key(path) {
                        return (
                            409,
                            Vec::new(),
                            br#"{"error":"exists","message":"exists"}"#.to_vec(),
                        );
                    }
                    state.records.insert(path.to_owned(), body);
                    if entry {
                        state.sequence += 1;
                        let sequence = state.sequence;
                        let key = path["entries/".len()..].to_owned();
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap()
                            .as_millis() as u64;
                        state.keys.insert(key, (now, sequence));
                    }
                    done()
                }
                _ => (405, Vec::new(), Vec::new()),
            }
        }
        _ => missing(),
    }
}
