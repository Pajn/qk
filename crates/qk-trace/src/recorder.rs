//! qk's side of the recorder: starts the helper, and turns the events it
//! forwards into each command's trace files.
//!
//! One thread reads the helper's output and hands each event to the worker
//! of the recording it belongs to, which writes that recording's trace. The
//! reader does no more than that, so that the helper, and the ring buffer
//! behind it, are drained as fast as events come: a worker that falls
//! behind holds its recording's events until it catches up, where a reader
//! that fell behind would leave the kernel program no room for them.

use std::collections::HashMap;
use std::io::{BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::calls::Arch;
use crate::helper::{EVENT, FAILED, FLUSHED, READY};
use crate::trace::{Event, Trace};

/// The prctl option a process names to join a recording: "qktr", which no
/// kernel defines, so that the call fails harmlessly once noticed.
const OPTION: u32 = 0x716b_7472;
/// How long a flush may take before the recording is given up on.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(60);
/// How many bytes of a recording's events the reader gathers before handing
/// them to its worker.
const BATCH: usize = 256 * 1024;

#[derive(Default)]
struct Shared {
    recordings: Mutex<HashMap<u32, Recorded>>,
    flushed: Mutex<Flushed>,
    signal: Condvar,
}

struct Recorded {
    queue: mpsc::Sender<Message>,
    worker: JoinHandle<Applied>,
    /// Events of no group the program could count apart, dropped before it
    /// began, by the helper's last count.
    unattributed: u64,
}

impl Recorded {
    /// Stops its worker without waiting for it.
    fn abandon(self) {
        let _ = self.queue.send(Message::End);
    }

    /// Stops its worker once it has applied what it was sent, and says what
    /// that left.
    fn end(self) -> Result<Applied> {
        let _ = self.queue.send(Message::End);
        self.worker
            .join()
            .map_err(|_| anyhow::anyhow!("the recorder's worker panicked"))
    }
}

/// What a recording's worker is sent.
enum Message {
    /// Events, each a little-endian `u32` length and that many bytes.
    Events(Vec<u8>),
    /// An event that may have been the recording's could not be read.
    Lost(String),
    /// Every event of the recording has been sent.
    End,
}

/// A recording's trace, and why it failed, if it did.
struct Applied {
    trace: Trace,
    failure: Option<String>,
}

/// Applies a recording's events to its trace, as they are sent.
fn work(mut trace: Trace, queue: mpsc::Receiver<Message>) -> Applied {
    let mut failure = None;
    for message in queue {
        match message {
            Message::Events(batch) if failure.is_none() => {
                let mut rest = batch.as_slice();
                while let Some((length, after)) = rest.split_first_chunk::<4>() {
                    let (payload, after) = after.split_at(u32::from_le_bytes(*length) as usize);
                    rest = after;
                    let applied = Event::parse(payload)
                        .map_err(|error| {
                            format!("the recorder sent an event qk cannot read: {error:#}")
                        })
                        .and_then(|event| trace.apply(event).map_err(|error| format!("{error:#}")));
                    if let Err(error) = applied {
                        failure = Some(error);
                        break;
                    }
                }
            }
            Message::Events(_) => {}
            Message::Lost(why) => {
                failure.get_or_insert(why);
            }
            Message::End => break,
        }
    }
    Applied { trace, failure }
}

/// A recording the reader has events of, and those it has yet to hand on.
struct Route {
    queue: mpsc::Sender<Message>,
    batch: Vec<u8>,
}

impl Route {
    /// Hands the gathered events on; false once the worker has stopped.
    fn send(&mut self) -> bool {
        self.batch.is_empty()
            || self
                .queue
                .send(Message::Events(std::mem::take(&mut self.batch)))
                .is_ok()
    }
}

#[derive(Default)]
struct Flushed {
    /// Each flush done, with how many of its group's events and how many
    /// unattributed ones had been dropped.
    done: HashMap<u64, (u64, u64)>,
    unattributed: u64,
    /// Why the helper stopped, when it has.
    stopped: Option<String>,
}

/// The recorder, its helper running.
pub struct Recorder {
    arch: Arch,
    input: Mutex<ChildStdin>,
    shared: Arc<Shared>,
    secret: u64,
    next_group: AtomicU32,
    next_flush: AtomicU64,
    _helper: Mutex<Child>,
}

impl std::fmt::Debug for Recorder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Recorder")
    }
}

/// One command's recording.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Session(u32);

/// Reads a frame's payload into `payload`, and says its kind.
fn read_frame(output: &mut impl Read, payload: &mut Vec<u8>) -> std::io::Result<Option<u8>> {
    let mut head = [0; 5];
    match output.read_exact(&mut head) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let length = u32::from_le_bytes(head[1..].try_into().expect("four bytes")) as usize;
    payload.resize(length, 0);
    output.read_exact(payload)?;
    Ok(Some(head[0]))
}

/// Reads the helper's frames until it stops, handing each event to the
/// worker of its recording.
fn read(mut output: impl Read, shared: &Shared) -> String {
    let mut routes: HashMap<u32, Route> = HashMap::new();
    let mut payload = Vec::new();
    loop {
        match read_frame(&mut output, &mut payload) {
            Ok(Some(EVENT)) => {
                // Its recording, or any it could be part of, is missing it.
                let Some(group) = payload.get(4..8) else {
                    for recorded in shared.recordings.lock().unwrap().values() {
                        let _ = recorded.queue.send(Message::Lost(
                            "the recorder sent an event qk cannot read: event cut short".to_owned(),
                        ));
                    }
                    continue;
                };
                let group = u32::from_le_bytes(group.try_into().expect("four bytes"));
                let route = match routes.entry(group) {
                    std::collections::hash_map::Entry::Occupied(route) => route.into_mut(),
                    std::collections::hash_map::Entry::Vacant(vacant) => {
                        let recordings = shared.recordings.lock().unwrap();
                        // No longer, or never, recorded.
                        let Some(recorded) = recordings.get(&group) else {
                            continue;
                        };
                        vacant.insert(Route {
                            queue: recorded.queue.clone(),
                            batch: Vec::new(),
                        })
                    }
                };
                route
                    .batch
                    .extend_from_slice(&(payload.len() as u32).to_le_bytes());
                route.batch.extend_from_slice(&payload);
                if route.batch.len() >= BATCH && !route.send() {
                    routes.remove(&group);
                }
            }
            Ok(Some(FLUSHED)) if payload.len() == 24 => {
                // Every event before the flush reaches its worker before the
                // recording that asked for it can end.
                let recordings = shared.recordings.lock().unwrap();
                routes.retain(|group, route| route.send() && recordings.contains_key(group));
                drop(recordings);
                let word =
                    |at: usize| u64::from_le_bytes(payload[at..at + 8].try_into().expect("eight"));
                let (id, dropped, unattributed) = (word(0), word(8), word(16));
                let mut flushed = shared.flushed.lock().unwrap();
                flushed.done.insert(id, (dropped, unattributed));
                flushed.unattributed = unattributed;
                shared.signal.notify_all();
            }
            Ok(Some(_)) => {}
            Ok(None) => return "the recorder's helper stopped".to_owned(),
            Err(error) => return format!("cannot read the recorder's helper: {error}"),
        }
    }
}

impl Recorder {
    /// Starts `helper`, which runs [`crate::helper::run`] with the
    /// privileges loading the program needs, and waits for it to attach.
    pub fn start(mut helper: Command) -> Result<Arc<Self>> {
        let arch = Arch::current().context("the recorder knows x86-64 and arm64 only")?;
        let mut secret = [0; 8];
        std::fs::File::open("/dev/urandom")?.read_exact(&mut secret)?;
        let secret = u64::from_le_bytes(secret);
        let mut child = helper
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("cannot start the recorder's helper")?;
        let mut input = child.stdin.take().context("no helper input")?;
        let mut output = BufReader::with_capacity(
            crate::helper::OUTPUT_BUFFER,
            child.stdout.take().context("no helper output")?,
        );
        writeln!(input, "{OPTION} {secret}")?;
        input.flush()?;
        let mut payload = Vec::new();
        match read_frame(&mut output, &mut payload)? {
            Some(READY) => {}
            Some(FAILED) => bail!("{}", String::from_utf8_lossy(&payload)),
            _ => {
                let mut stderr = String::new();
                if let Some(mut error) = child.stderr.take() {
                    let _ = error.read_to_string(&mut stderr);
                }
                let _ = child.wait();
                bail!("the recorder's helper did not start: {}", stderr.trim());
            }
        }
        let shared = Arc::new(Shared::default());
        let reading = shared.clone();
        std::thread::Builder::new()
            .name("qk-recorder".into())
            .spawn(move || {
                let stopped = read(output, &reading);
                reading.flushed.lock().unwrap().stopped = Some(stopped);
                reading.signal.notify_all();
            })
            .context("cannot start reading the recorder's helper")?;
        Ok(Arc::new(Self {
            arch,
            input: Mutex::new(input),
            shared,
            secret,
            next_group: AtomicU32::new(1),
            next_flush: AtomicU64::new(1),
            _helper: Mutex::new(child),
        }))
    }

    /// Begins recording a command, whose trace files go in `directory` as
    /// `<prefix>.<pid>`, and which starts in `cwd`. With `resolve`, an opened
    /// file's path is written with what its links lead to.
    pub fn begin(&self, directory: &Path, prefix: &str, cwd: &Path, resolve: bool) -> Session {
        let group = self.next_group.fetch_add(1, Ordering::Relaxed);
        let unattributed = self.shared.flushed.lock().unwrap().unattributed;
        let trace = Trace::new(self.arch, directory, prefix, cwd, resolve);
        let (queue, received) = mpsc::channel();
        let worker = std::thread::spawn(move || work(trace, received));
        self.shared.recordings.lock().unwrap().insert(
            group,
            Recorded {
                queue,
                worker,
                unattributed,
            },
        );
        Session(group)
    }

    /// What the command's process calls just before it execs, to join
    /// `session`: async-signal-safe, as `pre_exec` asks.
    pub fn join(
        &self,
        session: Session,
    ) -> impl Fn() -> std::io::Result<()> + Send + Sync + 'static {
        let secret = self.secret;
        move || {
            // SAFETY: prctl with integer arguments; an option no kernel
            // defines fails, and the recorder has seen it by then.
            unsafe {
                libc::prctl(
                    OPTION as libc::c_int,
                    secret as libc::c_ulong,
                    libc::c_ulong::from(session.0),
                    0 as libc::c_ulong,
                    0 as libc::c_ulong,
                );
            }
            Ok(())
        }
    }

    /// Drops `session` without waiting for its events.
    pub fn abandon(&self, session: Session) {
        if let Some(recorded) = self.shared.recordings.lock().unwrap().remove(&session.0) {
            recorded.abandon();
        }
    }

    /// Ends `session` once its command has finished: waits for every event
    /// the helper has, then writes out its trace. Fails when an event of it
    /// may have been lost.
    pub fn finish(&self, session: Session) -> Result<()> {
        let id = self.next_flush.fetch_add(1, Ordering::Relaxed);
        let flushed = {
            let mut input = self.input.lock().unwrap();
            writeln!(input, "flush {id} {}", session.0).and_then(|()| input.flush())
        };
        let recorded = || self.shared.recordings.lock().unwrap().remove(&session.0);
        if let Err(error) = flushed {
            if let Some(recorded) = recorded() {
                recorded.abandon();
            }
            bail!("cannot ask the recorder's helper to flush: {error}");
        }
        let mut state = self.shared.flushed.lock().unwrap();
        let (dropped, unattributed) = loop {
            if let Some(counts) = state.done.remove(&id) {
                break counts;
            }
            if let Some(stopped) = &state.stopped {
                let stopped = stopped.clone();
                drop(state);
                if let Some(recorded) = recorded() {
                    recorded.abandon();
                }
                bail!("{stopped}");
            }
            let (next, timeout) = self
                .shared
                .signal
                .wait_timeout(state, FLUSH_TIMEOUT)
                .unwrap();
            if timeout.timed_out() {
                drop(next);
                if let Some(recorded) = recorded() {
                    recorded.abandon();
                }
                bail!("the recorder's helper did not flush in time");
            }
            state = next;
        };
        drop(state);
        let recorded = recorded().context("the command was not being recorded")?;
        let before = recorded.unattributed;
        let applied = recorded.end()?;
        if let Some(failure) = applied.failure {
            bail!("{failure}");
        }
        // Only its own, or ones it may have been among.
        if dropped > 0 || unattributed > before {
            bail!("the recorder dropped events while the command ran");
        }
        applied.trace.finish()?;
        Ok(())
    }
}
