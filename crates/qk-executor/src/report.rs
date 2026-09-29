//! Where a run's progress, warnings and task output go. The runner and the
//! cache report events; the CLI installs a sink that renders them for the
//! output style. Without one, events print as `qk:` status lines on stderr.

use std::io::{self, Write};
use std::sync::OnceLock;

use crate::Outcome;

/// How a cacheable task's key was answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cached {
    Hit,
    RemoteHit,
    Miss,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A run with this many tasks begins.
    Planned {
        tasks: usize,
    },
    /// A finite task starts; `checking` when it consults the cache first.
    Started {
        id: String,
        checking: bool,
    },
    /// A continuous task starts; it runs until its dependents are done.
    StartedContinuous {
        id: String,
    },
    Cache {
        id: String,
        cached: Cached,
    },
    Finished {
        id: String,
        outcome: Outcome,
    },
    /// Not run because a dependency failed.
    Skipped {
        id: String,
    },
    Stopping {
        id: String,
    },
    Stopped {
        id: String,
    },
}

impl std::fmt::Display for Event {
    /// The `qk:` status line for the event, or nothing.
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::Planned { .. } => Ok(()),
            Self::Started { id, checking: true } => write!(f, "qk: checking {id}"),
            Self::Started {
                id,
                checking: false,
            } => write!(f, "qk: running {id}"),
            Self::StartedContinuous { id } => write!(f, "qk: started {id} (continuous)"),
            Self::Cache {
                id,
                cached: Cached::Hit,
            } => write!(f, "qk: cache hit {id}"),
            Self::Cache {
                id,
                cached: Cached::RemoteHit,
            } => write!(f, "qk: remote cache hit {id}"),
            Self::Cache {
                id,
                cached: Cached::Miss,
            } => write!(f, "qk: cache miss {id}"),
            Self::Finished {
                id,
                outcome: Outcome::Success,
            } => write!(f, "qk: finished {id}"),
            Self::Finished {
                id,
                outcome: Outcome::Failed(code),
            } => {
                write!(f, "qk: failed {id} (exit {code})")
            }
            Self::Finished {
                id,
                outcome: Outcome::Cancelled,
            } => write!(f, "qk: cancelled {id}"),
            Self::Skipped { id } => write!(f, "qk: skipped {id} (dependency failed)"),
            Self::Stopping { id } => write!(f, "qk: stopping {id} (no longer needed)"),
            Self::Stopped { id } => write!(f, "qk: stopped {id}"),
        }
    }
}

pub trait Sink: Send + Sync {
    fn event(&self, event: &Event);
    /// A `qk:` line about something that went wrong or was skipped.
    fn warning(&self, line: &str);
    /// Task output, to stdout or stderr.
    fn output(&self, stderr: bool, bytes: &[u8]);
}

/// Prints events and warnings as `qk:` lines on stderr, each in one write so
/// task processes sharing stderr cannot split them, and passes output through.
pub struct Lines;

impl Sink for Lines {
    fn event(&self, event: &Event) {
        let line = event.to_string();
        if !line.is_empty() {
            write_all(true, format!("{line}\n").as_bytes());
        }
    }

    fn warning(&self, line: &str) {
        write_all(true, format!("{line}\n").as_bytes());
    }

    fn output(&self, stderr: bool, bytes: &[u8]) {
        write_all(stderr, bytes);
    }
}

pub fn write_all(stderr: bool, bytes: &[u8]) {
    let _ = if stderr {
        io::stderr().lock().write_all(bytes)
    } else {
        io::stdout().lock().write_all(bytes)
    };
}

static SINK: OnceLock<Box<dyn Sink>> = OnceLock::new();

/// Installs the sink for the rest of the process; only the first one counts.
pub fn install(sink: Box<dyn Sink>) {
    let _ = SINK.set(sink);
}

fn sink() -> &'static dyn Sink {
    SINK.get().map_or(&Lines, |sink| sink.as_ref())
}

pub fn event(event: Event) {
    sink().event(&event);
}

pub fn warning(line: &str) {
    sink().warning(line);
}

pub fn output(stderr: bool, bytes: &[u8]) {
    if !bytes.is_empty() {
        sink().output(stderr, bytes);
    }
}
