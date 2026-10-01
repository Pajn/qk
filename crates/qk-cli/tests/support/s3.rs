//! An in-process stand-in for S3: path-style objects in memory over plain
//! HTTP, with signatures ignored. Enough to exercise qk's remote cache.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub struct State {
    pub objects: BTreeMap<String, Vec<u8>>,
    /// Methods and paths, in order.
    pub requests: Vec<String>,
    /// Close the connection halfway through each object read.
    pub cut_reads: bool,
    /// Answer listings with a server error.
    pub fail_lists: bool,
    /// The most objects one listing response names; all when zero.
    pub list_page: usize,
}

pub struct FakeS3 {
    pub endpoint: String,
    pub state: Arc<Mutex<State>>,
}

impl FakeS3 {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
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
        Self { endpoint, state }
    }

    pub fn objects(&self, prefix: &str) -> Vec<String> {
        let state = self.state.lock().unwrap();
        state
            .objects
            .keys()
            .filter(|key| key.starts_with(prefix))
            .cloned()
            .collect()
    }

    /// The requests made so far whose path starts with `prefix`.
    pub fn requests(&self, prefix: &str) -> Vec<String> {
        let state = self.state.lock().unwrap();
        state
            .requests
            .iter()
            .filter(|request| {
                request
                    .split_once(' ')
                    .is_some_and(|(_, path)| path.starts_with(prefix))
            })
            .cloned()
            .collect()
    }

    pub fn writes(&self) -> usize {
        let state = self.state.lock().unwrap();
        state
            .requests
            .iter()
            .filter(|request| request.starts_with("PUT"))
            .count()
    }
}

fn serve(stream: TcpStream, state: &Mutex<State>) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let target = parts.next().unwrap_or_default();
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let path = path.to_owned();
    let parameter = |name: &str| {
        query.split('&').find_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            (key == name).then(|| decode(value))
        })
    };
    let mut length = 0;
    let mut chunked = false;
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
            "transfer-encoding" => chunked = value.trim().eq_ignore_ascii_case("chunked"),
            _ => {}
        }
    }
    let mut body = Vec::new();
    if chunked {
        loop {
            let mut size = String::new();
            reader.read_line(&mut size)?;
            let size = usize::from_str_radix(size.trim(), 16).unwrap_or(0);
            let mut chunk = vec![0; size + 2];
            reader.read_exact(&mut chunk)?;
            if size == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..size]);
        }
    } else {
        body.resize(length, 0);
        reader.read_exact(&mut body)?;
    }
    let (status, response, cut) = {
        let mut state = state.lock().unwrap();
        state.requests.push(format!("{method} {path}"));
        match method.as_str() {
            "GET" if parameter("list-type").is_some() => {
                if state.fail_lists {
                    (500, Vec::new(), false)
                } else {
                    let bucket = format!("{}/", path.trim_end_matches('/'));
                    let prefix = format!("{bucket}{}", parameter("prefix").unwrap_or_default());
                    let after =
                        parameter("continuation-token").map(|token| format!("{bucket}{token}"));
                    let page = if state.list_page == 0 {
                        usize::MAX
                    } else {
                        state.list_page
                    };
                    let mut names = state
                        .objects
                        .keys()
                        .filter(|key| key.starts_with(&prefix))
                        .filter(|key| after.as_ref().is_none_or(|after| *key > after))
                        .map(|key| key[bucket.len()..].to_owned());
                    let listed: Vec<String> = names.by_ref().take(page).collect();
                    let more = names.next().is_some();
                    let mut xml = String::from(
                        "<ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">",
                    );
                    for name in &listed {
                        xml.push_str(&format!(
                            "<Contents><Key>{name}</Key><LastModified>2026-01-01T00:00:00.000Z</LastModified><ETag>\"x\"</ETag><Size>0</Size></Contents>"
                        ));
                    }
                    if more {
                        xml.push_str(&format!(
                            "<NextContinuationToken>{}</NextContinuationToken>",
                            listed.last().unwrap()
                        ));
                    }
                    xml.push_str("</ListBucketResult>");
                    (200, xml.into_bytes(), false)
                }
            }
            "DELETE" => {
                state.objects.remove(&path);
                (204, Vec::new(), false)
            }
            "PUT" if chunked => (501, Vec::new(), false),
            "PUT" => {
                state.objects.insert(path, body);
                (200, Vec::new(), false)
            }
            "GET" | "HEAD" => match state.objects.get(&path) {
                Some(object) => (200, object.clone(), method == "GET" && state.cut_reads),
                None => (404, Vec::new(), false),
            },
            _ => (405, Vec::new(), false),
        }
    };
    let mut stream = stream;
    write!(
        stream,
        "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.len()
    )?;
    if cut {
        stream.write_all(&response[..response.len() / 2])?;
    } else if method != "HEAD" {
        stream.write_all(&response)?;
    }
    stream.flush()
}

/// A query value with its percent-escapes decoded.
fn decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let escape = bytes
            .get(index + 1..index + 3)
            .filter(|_| bytes[index] == b'%')
            .and_then(|hex| u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok());
        match escape {
            Some(byte) => {
                decoded.push(byte);
                index += 3;
            }
            None => {
                decoded.push(bytes[index]);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}
