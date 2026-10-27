//! qk's side of the recorder: starts the helper, and turns the events it
//! forwards into each command's trace files.

use std::collections::HashMap;
use std::io::{BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
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

#[derive(Default)]
struct Shared {
    traces: Mutex<HashMap<u32, Recorded>>,
    flushed: Mutex<Flushed>,
    signal: Condvar,
}

struct Recorded {
    trace: Trace,
    /// Events of no group the program could count apart, dropped before it
    /// began, by the helper's last count.
    unattributed: u64,
    failure: Option<String>,
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

fn read_frame(output: &mut impl Read) -> std::io::Result<Option<(u8, Vec<u8>)>> {
    let mut head = [0; 5];
    match output.read_exact(&mut head) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let length = u32::from_le_bytes(head[1..].try_into().expect("four bytes")) as usize;
    let mut payload = vec![0; length];
    output.read_exact(&mut payload)?;
    Ok(Some((head[0], payload)))
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
        let mut output = BufReader::new(child.stdout.take().context("no helper output")?);
        writeln!(input, "{OPTION} {secret}")?;
        input.flush()?;
        match read_frame(&mut output)? {
            Some((READY, _)) => {}
            Some((FAILED, message)) => bail!("{}", String::from_utf8_lossy(&message)),
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
        std::thread::spawn(move || {
            let stopped = loop {
                match read_frame(&mut output) {
                    Ok(Some((EVENT, payload))) => {
                        let mut traces = reading.traces.lock().unwrap();
                        let event = match Event::parse(&payload) {
                            Ok(event) => event,
                            // Its recording, or any it could be part of, is
                            // missing it.
                            Err(error) => {
                                let group = payload.get(4..8).map(|group| {
                                    u32::from_le_bytes(group.try_into().expect("four"))
                                });
                                for (_, recorded) in traces
                                    .iter_mut()
                                    .filter(|(each, _)| group.is_none_or(|group| **each == group))
                                {
                                    recorded.failure.get_or_insert_with(|| {
                                        format!(
                                            "the recorder sent an event qk cannot read: {error:#}"
                                        )
                                    });
                                }
                                continue;
                            }
                        };
                        if let Some(recorded) = traces.get_mut(&event.group())
                            && recorded.failure.is_none()
                            && let Err(error) = recorded.trace.apply(event)
                        {
                            recorded.failure = Some(format!("{error:#}"));
                        }
                    }
                    Ok(Some((FLUSHED, payload))) if payload.len() == 24 => {
                        let word = |at: usize| {
                            u64::from_le_bytes(payload[at..at + 8].try_into().expect("eight"))
                        };
                        let (id, dropped, unattributed) = (word(0), word(8), word(16));
                        let mut flushed = reading.flushed.lock().unwrap();
                        flushed.done.insert(id, (dropped, unattributed));
                        flushed.unattributed = unattributed;
                        reading.signal.notify_all();
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => break "the recorder's helper stopped".to_owned(),
                    Err(error) => break format!("cannot read the recorder's helper: {error}"),
                }
            };
            reading.flushed.lock().unwrap().stopped = Some(stopped);
            reading.signal.notify_all();
        });
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
        self.shared.traces.lock().unwrap().insert(
            group,
            Recorded {
                trace: Trace::new(self.arch, directory, prefix, cwd, resolve),
                unattributed,
                failure: None,
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
        self.shared.traces.lock().unwrap().remove(&session.0);
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
        let recorded = || self.shared.traces.lock().unwrap().remove(&session.0);
        if let Err(error) = flushed {
            recorded();
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
                recorded();
                bail!("{stopped}");
            }
            let (next, timeout) = self
                .shared
                .signal
                .wait_timeout(state, FLUSH_TIMEOUT)
                .unwrap();
            if timeout.timed_out() {
                drop(next);
                recorded();
                bail!("the recorder's helper did not flush in time");
            }
            state = next;
        };
        drop(state);
        let recorded = recorded().context("the command was not being recorded")?;
        if let Some(failure) = recorded.failure {
            bail!("{failure}");
        }
        // Only its own, or ones it may have been among.
        if dropped > 0 || unattributed > recorded.unattributed {
            bail!("the recorder dropped events while the command ran");
        }
        recorded.trace.finish()?;
        Ok(())
    }
}
