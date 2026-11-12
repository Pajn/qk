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
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
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
/// How many bytes of events, of every recording together, may wait for
/// their workers: an eighth of the machine's memory. A recording whose
/// events would go past it fails, as one whose events the kernel program
/// dropped does, rather than the recorder growing without bound.
fn backlog_limit() -> usize {
    // SAFETY: sysconf has no preconditions.
    let (pages, size) = unsafe {
        (
            libc::sysconf(libc::_SC_PHYS_PAGES),
            libc::sysconf(libc::_SC_PAGESIZE),
        )
    };
    usize::try_from(pages)
        .ok()
        .zip(usize::try_from(size).ok())
        .map_or(1 << 30, |(pages, size)| pages.saturating_mul(size) / 8)
}

#[derive(Default)]
struct Shared {
    recordings: Mutex<HashMap<u32, Recorded>>,
    flushed: Mutex<Flushed>,
    signal: Condvar,
    /// How many bytes of events wait for their workers.
    backlog: Arc<AtomicUsize>,
    /// How many may, by [`backlog_limit`].
    limit: usize,
}

struct Recorded {
    queue: mpsc::Sender<Message>,
    worker: JoinHandle<Applied>,
    /// Set once nothing is to be made of the events its worker has left.
    abandoned: Arc<AtomicBool>,
    /// Events of no group the program could count apart, dropped before it
    /// began, by the helper's last count.
    unattributed: u64,
}

impl Recorded {
    /// Stops its worker without waiting for it, or for it to apply what it
    /// was sent.
    fn abandon(self) {
        self.abandoned.store(true, Ordering::Relaxed);
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
    Events(Batch),
    /// An event that may have been the recording's could not be read.
    Lost(String),
    /// Every event of the recording has been sent.
    End,
}

/// Events, each a little-endian `u32` length and that many bytes, counted
/// in the backlog until they are dropped, applied or not.
struct Batch {
    bytes: Vec<u8>,
    backlog: Arc<AtomicUsize>,
}

impl Drop for Batch {
    fn drop(&mut self) {
        self.backlog.fetch_sub(self.bytes.len(), Ordering::Relaxed);
    }
}

/// A recording's trace, and why it failed, if it did.
struct Applied {
    trace: Trace,
    failure: Option<String>,
}

/// Applies a recording's events to its trace, as they are sent, until
/// `abandoned`.
fn work(mut trace: Trace, queue: mpsc::Receiver<Message>, abandoned: &AtomicBool) -> Applied {
    let mut failure = None;
    for message in queue {
        match message {
            Message::Events(batch) if failure.is_none() && !abandoned.load(Ordering::Relaxed) => {
                let mut rest = batch.bytes.as_slice();
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
    /// Whether events of it went past the backlog, after which the rest are
    /// not kept.
    lost: bool,
}

impl Route {
    /// Hands the gathered events on, unless they would take the backlog
    /// past `limit`; false once the worker has stopped.
    fn send(&mut self, backlog: &Arc<AtomicUsize>, limit: usize) -> bool {
        let size = self.batch.len();
        if size == 0 {
            return true;
        }
        if backlog.fetch_add(size, Ordering::Relaxed) + size > limit {
            backlog.fetch_sub(size, Ordering::Relaxed);
            self.batch = Vec::new();
            self.lost = true;
            return self
                .queue
                .send(Message::Lost(
                    "the recorder fell too far behind the command's events".to_owned(),
                ))
                .is_ok();
        }
        self.queue
            .send(Message::Events(Batch {
                bytes: std::mem::take(&mut self.batch),
                backlog: backlog.clone(),
            }))
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
                            lost: false,
                        })
                    }
                };
                if route.lost {
                    continue;
                }
                route
                    .batch
                    .extend_from_slice(&(payload.len() as u32).to_le_bytes());
                route.batch.extend_from_slice(&payload);
                if route.batch.len() >= BATCH && !route.send(&shared.backlog, shared.limit) {
                    routes.remove(&group);
                }
            }
            Ok(Some(FLUSHED)) if payload.len() == 24 => {
                // Every event before the flush reaches its worker before the
                // recording that asked for it can end.
                let recordings = shared.recordings.lock().unwrap();
                routes.retain(|group, route| {
                    route.send(&shared.backlog, shared.limit) && recordings.contains_key(group)
                });
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
        let shared = Arc::new(Shared {
            limit: backlog_limit(),
            ..Shared::default()
        });
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
        let abandoned = Arc::new(AtomicBool::new(false));
        let worker = {
            let abandoned = abandoned.clone();
            std::thread::spawn(move || work(trace, received, &abandoned))
        };
        self.shared.recordings.lock().unwrap().insert(
            group,
            Recorded {
                queue,
                worker,
                abandoned,
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
        // Only its own, or ones it may have been among; known without its
        // worker, so it is not waited for.
        if dropped > 0 || unattributed > recorded.unattributed {
            recorded.abandon();
            bail!("the recorder dropped events while the command ran");
        }
        let applied = recorded.end()?;
        if let Some(failure) = applied.failure {
            bail!("{failure}");
        }
        applied.trace.finish()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route() -> (Route, mpsc::Receiver<Message>) {
        let (queue, received) = mpsc::channel();
        let route = Route {
            queue,
            batch: vec![0; 100],
            lost: false,
        };
        (route, received)
    }

    #[test]
    fn a_batch_counts_in_the_backlog_until_it_is_dropped() {
        let backlog = Arc::new(AtomicUsize::new(0));
        let (mut route, received) = route();
        assert!(route.send(&backlog, 1000));
        assert_eq!(backlog.load(Ordering::Relaxed), 100);
        let Ok(Message::Events(batch)) = received.recv() else {
            panic!("no events sent");
        };
        drop(batch);
        assert_eq!(backlog.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn events_past_the_backlog_fail_their_recording_and_are_not_kept() {
        let backlog = Arc::new(AtomicUsize::new(950));
        let (mut route, received) = route();
        assert!(route.send(&backlog, 1000));
        assert!(route.lost);
        assert!(route.batch.is_empty());
        assert_eq!(backlog.load(Ordering::Relaxed), 950);
        assert!(matches!(received.recv(), Ok(Message::Lost(_))));
    }

    #[test]
    fn a_batch_its_worker_never_received_leaves_the_backlog() {
        let backlog = Arc::new(AtomicUsize::new(0));
        let (mut route, received) = route();
        drop(received);
        assert!(!route.send(&backlog, 1000));
        assert_eq!(backlog.load(Ordering::Relaxed), 0);
    }
}
