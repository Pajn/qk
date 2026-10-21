//! The other end of a `signal` readiness check, as Nx 24 has it: a socket
//! qk names to the task in `NX_READY_SIGNAL_SOCKET`, with the task's id in
//! `NX_READY_SIGNAL_TASK`, on which `@nx/devkit`'s `signalTaskReady` reports
//! that the task is ready and the address it listens on. That is a Unix
//! socket, or a named pipe on Windows.
//!
//! Nx frames each message as `NX_MSG_<length>:` and then the payload, which
//! is JSON unless `NX_USE_V8_SERIALIZER` is set: qk cannot read Node's own
//! serialization, so such a task's signal is not seen.

use std::ffi::OsString;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::JoinHandle;

use anyhow::Result;
use serde_json::Value;

use crate::readiness::Verdict;

/// Names the socket to the task.
pub const SOCKET_ENV: &str = "NX_READY_SIGNAL_SOCKET";
/// Names the task to itself, which its signal repeats.
pub const TASK_ENV: &str = "NX_READY_SIGNAL_TASK";
/// The address a server a task waited for reported, for that task.
pub const URL_ENV: &str = "NX_READY_URL";

const PREFIX: &[u8] = b"NX_MSG_";
/// Longer than any length a message here could declare: a stream that runs
/// past it without a header is not one qk can read.
const MAX_HEADER: usize = PREFIX.len() + 16 + 1;

static NEXT: AtomicUsize = AtomicUsize::new(0);

/// Receives one task's signal while it is kept, and stops when dropped.
pub struct Listener {
    address: OsString,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Listener {
    pub fn start(task: &str, verdict: Arc<Verdict>) -> Result<Self> {
        let name = format!(
            "qk-ready-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let stop = Arc::new(AtomicBool::new(false));
        let reader = Reader {
            task: task.to_owned(),
            verdict,
        };
        let (address, thread) = platform::listen(&name, reader, stop.clone())?;
        Ok(Self {
            address,
            stop,
            thread: Some(thread),
        })
    }

    /// The variables that tell the task where to signal.
    pub fn env(&self, task: &str) -> [(OsString, OsString); 2] {
        [
            (SOCKET_ENV.into(), self.address.clone()),
            (TASK_ENV.into(), task.into()),
        ]
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        platform::wake(&self.address, self.thread.as_ref());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        platform::remove(&self.address);
    }
}

/// Reads the messages of one connection.
struct Reader {
    task: String,
    verdict: Arc<Verdict>,
}

impl Reader {
    /// Takes every whole message from `buffer`, and says whether the stream
    /// can still be read: one that is not framed as Nx frames is dropped.
    fn consume(&self, buffer: &mut Vec<u8>) -> bool {
        loop {
            let header = PREFIX.len().min(buffer.len());
            if buffer[..header] != PREFIX[..header] {
                return false;
            }
            let Some(colon) = buffer
                .iter()
                .take(MAX_HEADER)
                .position(|&byte| byte == b':')
            else {
                return buffer.len() < MAX_HEADER;
            };
            let Some(length) = std::str::from_utf8(&buffer[PREFIX.len()..colon])
                .ok()
                .and_then(|digits| digits.parse::<usize>().ok())
            else {
                return false;
            };
            let end = colon + 1 + length;
            if buffer.len() < end {
                return true;
            }
            self.message(&buffer[colon + 1..end]);
            buffer.drain(..end);
        }
    }

    /// `{"type": "TO_PARENT_FROM_CHILDREN", "message": {"type":
    /// "nx-ready-signal:ready", "taskId": …, "url": …}}` makes the task ready.
    fn message(&self, payload: &[u8]) {
        let Ok(envelope) = serde_json::from_slice::<Value>(payload) else {
            return;
        };
        let message = &envelope["message"];
        if envelope["type"] == "TO_PARENT_FROM_CHILDREN"
            && message["type"] == "nx-ready-signal:ready"
            && message["taskId"] == self.task.as_str()
        {
            let url = message["url"].as_str().map(str::to_owned);
            self.verdict.set_ready_at(url);
        }
    }
}

#[cfg(unix)]
mod platform {
    use std::ffi::OsString;
    use std::io::{ErrorKind, Read};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread::JoinHandle;

    use anyhow::{Context, Result};

    use super::Reader;

    const POLL: std::time::Duration = std::time::Duration::from_millis(20);

    pub fn listen(
        name: &str,
        reader: Reader,
        stop: Arc<AtomicBool>,
    ) -> Result<(OsString, JoinHandle<()>)> {
        // A socket's path has room for about a hundred bytes, which a long
        // temporary directory can take.
        let mut path = std::env::temp_dir().join(format!("{name}.sock"));
        if path.as_os_str().len() > 100 {
            path = PathBuf::from("/tmp").join(format!("{name}.sock"));
        }
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path)
            .with_context(|| format!("cannot listen for a ready signal at {}", path.display()))?;
        listener.set_nonblocking(true)?;
        let thread = std::thread::spawn(move || {
            let mut streams: Vec<(UnixStream, Vec<u8>)> = Vec::new();
            while !stop.load(Ordering::SeqCst) {
                while let Ok((stream, _)) = listener.accept() {
                    if stream.set_nonblocking(true).is_ok() {
                        streams.push((stream, Vec::new()));
                    }
                }
                streams.retain_mut(|(stream, buffer)| {
                    let mut chunk = [0; 4096];
                    loop {
                        match stream.read(&mut chunk) {
                            Ok(0) => return false,
                            Ok(read) => {
                                buffer.extend_from_slice(&chunk[..read]);
                                if !reader.consume(buffer) {
                                    return false;
                                }
                            }
                            Err(error) if error.kind() == ErrorKind::WouldBlock => return true,
                            Err(error) if error.kind() == ErrorKind::Interrupted => {}
                            Err(_) => return false,
                        }
                    }
                });
                std::thread::sleep(POLL);
            }
        });
        Ok((path.into_os_string(), thread))
    }

    /// The listener polls, so it sees the stop on its own.
    pub fn wake(_: &OsString, _: Option<&JoinHandle<()>>) {}

    pub fn remove(address: &OsString) {
        let _ = std::fs::remove_file(Path::new(address));
    }
}

#[cfg(windows)]
mod platform {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStrExt;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread::JoinHandle;

    use anyhow::{Result, bail};
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_PIPE_CONNECTED, GENERIC_WRITE, GetLastError, HANDLE,
        INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, OPEN_EXISTING, PIPE_ACCESS_INBOUND, ReadFile,
    };
    use windows_sys::Win32::System::IO::CancelSynchronousIo;
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
        PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };

    use super::Reader;

    fn wide(address: &OsString) -> Vec<u16> {
        address.encode_wide().chain(Some(0)).collect()
    }

    fn instance(address: &OsString) -> HANDLE {
        let name = wide(address);
        // SAFETY: `name` is a NUL-terminated wide string that outlives the call.
        unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_INBOUND,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                0,
                65536,
                0,
                std::ptr::null(),
            )
        }
    }

    pub fn listen(
        name: &str,
        reader: Reader,
        stop: Arc<AtomicBool>,
    ) -> Result<(OsString, JoinHandle<()>)> {
        let address = OsString::from(format!(r"\\.\pipe\{name}"));
        // The first instance exists before the task starts, so it can connect.
        let first = instance(&address);
        if first == INVALID_HANDLE_VALUE {
            bail!("cannot listen for a ready signal at {}", address.display());
        }
        let pipe = address.clone();
        // A handle is a pointer, which a thread cannot take as it is.
        let first = first as isize;
        let thread = std::thread::spawn(move || {
            let mut handle = first as HANDLE;
            while !stop.load(Ordering::SeqCst) {
                // SAFETY: `handle` is a pipe instance this thread owns.
                let connected = unsafe { ConnectNamedPipe(handle, std::ptr::null_mut()) } != 0
                    || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED;
                if connected && !stop.load(Ordering::SeqCst) {
                    let mut buffer = Vec::new();
                    let mut chunk = [0u8; 4096];
                    // A read starting after the stop would not be cancelled.
                    while !stop.load(Ordering::SeqCst) {
                        let mut read = 0;
                        // SAFETY: `chunk` is writable for its length, and
                        // `read` receives how much was written.
                        let ok = unsafe {
                            ReadFile(
                                handle,
                                chunk.as_mut_ptr(),
                                chunk.len() as u32,
                                &mut read,
                                std::ptr::null_mut(),
                            )
                        } != 0;
                        if !ok || read == 0 {
                            break;
                        }
                        buffer.extend_from_slice(&chunk[..read as usize]);
                        if !reader.consume(&mut buffer) {
                            break;
                        }
                    }
                }
                // SAFETY: closed once, and replaced before it is used again.
                unsafe { CloseHandle(handle) };
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                handle = instance(&pipe);
                if handle == INVALID_HANDLE_VALUE {
                    return;
                }
            }
            // SAFETY: the instance made for the next connection, unused.
            unsafe { CloseHandle(handle) };
        });
        Ok((address, thread))
    }

    /// Cancels the listener's blocking read or connect, and connects to it,
    /// until it sees the stop and ends. A task holding its connection open
    /// would otherwise keep the listener reading, with the pipe too busy to
    /// connect to, and a read it began just after a cancellation would wait.
    pub fn wake(address: &OsString, thread: Option<&JoinHandle<()>>) {
        let name = wide(address);
        // Until the listener ends: a read it began after one cancellation
        // is cancelled by the next, and a connection it waits for is made.
        loop {
            if let Some(thread) = thread {
                if thread.is_finished() {
                    return;
                }
                use std::os::windows::io::AsRawHandle;
                // SAFETY: the handle is the listener thread's, not yet joined.
                unsafe { CancelSynchronousIo(thread.as_raw_handle()) };
            }
            // SAFETY: `name` is a NUL-terminated wide string that outlives
            // the call.
            let handle = unsafe {
                CreateFileW(
                    name.as_ptr(),
                    GENERIC_WRITE,
                    0,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    0,
                    std::ptr::null_mut(),
                )
            };
            if handle != INVALID_HANDLE_VALUE {
                // SAFETY: the handle was just opened here.
                unsafe { CloseHandle(handle) };
            }
            if thread.is_none() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// A named pipe goes with its last handle.
    pub fn remove(_: &OsString) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(payload: &str) -> Vec<u8> {
        let mut bytes = format!("NX_MSG_{}:", payload.len()).into_bytes();
        bytes.extend_from_slice(payload.as_bytes());
        bytes
    }

    #[test]
    fn reads_a_signal_split_across_reads() {
        let verdict = Arc::new(Verdict::default());
        let reader = Reader {
            task: "app:serve".into(),
            verdict: verdict.clone(),
        };
        let mut bytes = frame(r#"{"type":"CHILD_READY","message":"x"}"#);
        // Another task's signal, then this one's.
        bytes.extend(frame(
            r#"{"type":"TO_PARENT_FROM_CHILDREN","message":{"type":"nx-ready-signal:ready","taskId":"app:other"}}"#,
        ));
        bytes.extend(frame(
            r#"{"type":"TO_PARENT_FROM_CHILDREN","message":{"type":"nx-ready-signal:ready","taskId":"app:serve","url":"http://localhost:4200/"}}"#,
        ));
        let mut buffer = Vec::new();
        for chunk in bytes.chunks(7) {
            buffer.extend_from_slice(chunk);
            assert!(reader.consume(&mut buffer));
        }
        assert!(buffer.is_empty());
        assert_eq!(verdict.url().as_deref(), Some("http://localhost:4200/"));
    }

    /// Connects as libuv does, read-write first and write-only when the
    /// inbound pipe refuses that, and writes the header and payload apart,
    /// as Node does, many times over.
    #[cfg(windows)]
    #[test]
    fn hears_signals_on_a_named_pipe_as_node_sends_them() {
        use std::io::Write;
        let payload = r#"{"type":"TO_PARENT_FROM_CHILDREN","message":{"type":"nx-ready-signal:ready","taskId":"app:serve"}}"#;
        for attempt in 0..200 {
            let verdict = Arc::new(Verdict::default());
            let listener = Listener::start("app:serve", verdict.clone()).unwrap();
            let address = listener.address.clone();
            assert!(
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&address)
                    .is_err(),
                "an inbound pipe takes no reader"
            );
            let mut pipe = std::fs::OpenOptions::new()
                .write(true)
                .open(&address)
                .unwrap_or_else(|error| panic!("attempt {attempt}: {error}"));
            pipe.write_all(format!("NX_MSG_{}:", payload.len()).as_bytes())
                .unwrap();
            pipe.write_all(payload.as_bytes()).unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !verdict.is_ready() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            assert!(verdict.is_ready(), "attempt {attempt}: no signal heard");
            drop(pipe);
            drop(listener);
        }
    }

    #[test]
    fn drops_a_stream_not_framed_as_nx_frames_it() {
        let reader = Reader {
            task: "app:serve".into(),
            verdict: Arc::new(Verdict::default()),
        };
        assert!(!reader.consume(&mut b"hello".to_vec()));
        assert!(!reader.consume(&mut b"NX_MSG_x:".to_vec()));
        assert!(reader.consume(&mut b"NX_MSG_12".to_vec()));
    }
}
