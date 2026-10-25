//! qk's eBPF recorder: what a task's processes read, from the kernel, as
//! strace would have written it. See [`trace`] for the format, and
//! `bpf/trace.bpf.c` for the kernel program.
//!
//! Loading the program takes privileges qk does not run with, so a helper
//! loads it, [`helper::run`], and forwards its events to a [`Recorder`].

pub mod calls;
#[cfg(target_os = "linux")]
pub mod helper;
#[cfg(target_os = "linux")]
mod program;
#[cfg(target_os = "linux")]
mod recorder;
#[cfg(unix)]
pub mod trace;

#[cfg(target_os = "linux")]
pub use recorder::{Recorder, Session};
