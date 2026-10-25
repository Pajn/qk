//! The recorder's events, and the strace trace files they become.
//!
//! Each event is one of the kernel program's: a process joining a recording,
//! a recorded process forking or exiting, or a recorded call returning, with
//! its arguments, the paths they name and its result. A [`Trace`] keeps each process's
//! descriptors and working directory as its calls change them, and writes
//! every call as `strace -ff -ttt -yy` writes it: one file per process, a
//! descriptor with the path it was opened at, so that qk's strace decoder
//! reads a recording the same whichever recorder made it.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::calls::{Arch, Arg, Call, Effect};

/// The longest path the kernel program reads.
pub const PATH_BYTES: usize = 4096;
const AT_FDCWD: i32 = -100;

const BEGIN: u8 = 1;
const FORK: u8 = 2;
const CALL: u8 = 3;
const EXIT: u8 = 4;

const HEADER: usize = 24;

/// One of the kernel program's events.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Begin {
        group: u32,
        tgid: u32,
        time: u64,
    },
    Fork {
        group: u32,
        parent: u32,
        child: u32,
        time: u64,
    },
    /// A call, timed as it entered, and when it returned.
    Call {
        group: u32,
        tgid: u32,
        time: u64,
        exited: u64,
        call: u16,
        args: [u64; 6],
        paths: [PathArg; 2],
        result: i64,
    },
    /// A process's last thread exiting.
    Exit {
        group: u32,
        tgid: u32,
        time: u64,
    },
}

/// A path argument as the kernel program read it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathArg {
    /// No path argument, or a null pointer.
    Null,
    /// The path, without its terminating NUL. `cut` when it filled the
    /// buffer, which leaves it unknown whether that was all of it.
    Read { bytes: Vec<u8>, cut: bool },
    /// A pointer the program could not read.
    Unreadable,
}

impl Event {
    /// The group the event belongs to.
    pub fn group(&self) -> u32 {
        match self {
            Self::Begin { group, .. }
            | Self::Fork { group, .. }
            | Self::Call { group, .. }
            | Self::Exit { group, .. } => *group,
        }
    }

    /// An event as the kernel program writes it.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let u16_at = |at: usize| -> Result<u16> {
            Ok(u16::from_le_bytes(
                bytes
                    .get(at..at + 2)
                    .context("event cut short")?
                    .try_into()?,
            ))
        };
        let u32_at = |at: usize| -> Result<u32> {
            Ok(u32::from_le_bytes(
                bytes
                    .get(at..at + 4)
                    .context("event cut short")?
                    .try_into()?,
            ))
        };
        let u64_at = |at: usize| -> Result<u64> {
            Ok(u64::from_le_bytes(
                bytes
                    .get(at..at + 8)
                    .context("event cut short")?
                    .try_into()?,
            ))
        };
        let kind = *bytes.first().context("empty event")?;
        let call = u16_at(2)?;
        let group = u32_at(4)?;
        let tgid = u32_at(8)?;
        let time = u64_at(16)?;
        Ok(match kind {
            BEGIN => Self::Begin { group, tgid, time },
            EXIT => Self::Exit { group, tgid, time },
            FORK => Self::Fork {
                group,
                parent: tgid,
                child: u32_at(HEADER)?,
                time,
            },
            CALL => {
                let mut args = [0; 6];
                for (index, arg) in args.iter_mut().enumerate() {
                    *arg = u64_at(HEADER + index * 8)?;
                }
                let result = u64_at(HEADER + 48)? as i64;
                let exited = u64_at(HEADER + 56)?;
                let lengths = [u16_at(HEADER + 64)?, u16_at(HEADER + 66)?];
                let mut at = HEADER + 72;
                let mut paths = [PathArg::Null, PathArg::Null];
                for (path, length) in paths.iter_mut().zip(lengths) {
                    let length = usize::from(length);
                    *path = match length {
                        0 => PathArg::Null,
                        _ if length > PATH_BYTES => PathArg::Unreadable,
                        _ => {
                            let read = bytes.get(at..at + length).context("path cut short")?;
                            at += length;
                            PathArg::Read {
                                bytes: read[..length - 1].to_vec(),
                                cut: length == PATH_BYTES,
                            }
                        }
                    };
                }
                Self::Call {
                    group,
                    tgid,
                    time,
                    exited,
                    call,
                    args,
                    paths,
                    result,
                }
            }
            other => bail!("unknown event kind {other}"),
        })
    }
}

#[derive(Default)]
struct Process {
    cwd: PathBuf,
    descriptors: HashMap<i32, PathBuf>,
    /// The call that last gave out each descriptor number.
    given: HashMap<i32, Given>,
    /// When the last call seen to give out or close each number returned.
    touched: HashMap<i32, u64>,
    file: Option<BufWriter<File>>,
}

/// A call that gave out a descriptor number, and when it entered and
/// returned.
#[derive(Clone, Copy)]
struct Given {
    entered: u64,
    exited: u64,
    /// Whether it named the number itself, as dup2 does, rather than being
    /// given the lowest free one.
    named: bool,
}

/// A call's arguments and paths, and when it entered and returned.
struct Entered {
    args: [u64; 6],
    paths: [PathArg; 2],
    time: u64,
    exited: u64,
}

/// One command's recording: what its processes did, written as strace's
/// trace files, `<prefix>.<pid>`, in a directory.
pub struct Trace {
    arch: Arch,
    directory: PathBuf,
    prefix: String,
    /// The working directory the command started in.
    cwd: PathBuf,
    /// Whether an opened file's path is written with what its links lead
    /// to, as `-yy` writes it, which costs a lookup an open.
    resolve: bool,
    processes: HashMap<u32, Process>,
}

impl Trace {
    pub fn new(arch: Arch, directory: &Path, prefix: &str, cwd: &Path, resolve: bool) -> Self {
        Self {
            arch,
            directory: directory.to_owned(),
            prefix: prefix.to_owned(),
            cwd: cwd.to_owned(),
            resolve,
            processes: HashMap::new(),
        }
    }

    fn process(&mut self, tgid: u32) -> &mut Process {
        let cwd = self.cwd.clone();
        self.processes.entry(tgid).or_insert_with(|| Process {
            cwd,
            ..Process::default()
        })
    }

    /// Writes `line`, timed `time`, to the trace of `tgid`.
    fn write(&mut self, tgid: u32, time: u64, line: &str) -> Result<()> {
        let path = self.directory.join(format!("{}.{tgid}", self.prefix));
        let process = self.process(tgid);
        if process.file.is_none() {
            // Appended to, should a later process be given the same id; the
            // directory starts empty.
            let file = File::options()
                .create(true)
                .append(true)
                .open(&path)
                .with_context(|| format!("cannot write {}", path.display()))?;
            process.file = Some(BufWriter::new(file));
        }
        let file = process.file.as_mut().expect("opened above");
        writeln!(
            file,
            "{}.{:06} {line}",
            time / 1_000_000_000,
            (time % 1_000_000_000) / 1000
        )?;
        Ok(())
    }

    pub fn apply(&mut self, event: Event) -> Result<()> {
        match event {
            Event::Begin { tgid, .. } => {
                let cwd = self.cwd.clone();
                let process = self.process(tgid);
                process.cwd = cwd;
            }
            Event::Fork {
                parent,
                child,
                time,
                ..
            } => {
                let inherited = {
                    let parent = self.process(parent);
                    (
                        parent.cwd.clone(),
                        parent.descriptors.clone(),
                        parent.given.clone(),
                        parent.touched.clone(),
                    )
                };
                let process = self.process(child);
                process.cwd = inherited.0;
                process.descriptors = inherited.1;
                process.given = inherited.2;
                process.touched = inherited.3;
                // As strace writes the call that made the child, which the
                // decoder hands the parent's working directory to.
                self.write(
                    parent,
                    time,
                    &format!("clone(child_stack=NULL, flags=SIGCHLD) = {child}"),
                )?;
            }
            // Its trace is complete, and its file closed, so that a command
            // running many processes keeps only the live ones' open.
            Event::Exit { tgid, .. } => {
                if let Some(mut file) = self.processes.remove(&tgid).and_then(|p| p.file) {
                    file.flush()?;
                }
            }
            Event::Call {
                tgid,
                time,
                exited,
                call,
                args,
                paths,
                result,
                ..
            } => {
                let Some(spec) = self.arch.call(call) else {
                    return Ok(());
                };
                let entered = Entered {
                    args,
                    paths,
                    time,
                    exited,
                };
                if spec.written {
                    let (arch, resolve) = (self.arch, self.resolve);
                    let process = self.process(tgid);
                    let opened = (resolve && spec.effect == Effect::Opens && result >= 0)
                        .then(|| opened(spec, &entered, process))
                        .flatten()
                        .and_then(|path| std::fs::canonicalize(path).ok());
                    let line = render(arch, spec, &entered, result, process, opened.as_deref());
                    self.write(tgid, time, &line)?;
                }
                if result >= 0 {
                    let process = self.process(tgid);
                    effect(spec, &entered, result, process)?;
                }
            }
        }
        Ok(())
    }

    /// Writes out what is buffered, and says where the trace is.
    pub fn finish(mut self) -> Result<PathBuf> {
        for process in self.processes.values_mut() {
            if let Some(file) = process.file.as_mut() {
                file.flush()?;
            }
        }
        Ok(self.directory)
    }
}

/// The paths a call names, in the order its arguments do.
fn path_of(entered: &Entered, nth: usize) -> Option<&[u8]> {
    match &entered.paths[nth] {
        PathArg::Read { bytes, cut: false } => Some(bytes),
        _ => None,
    }
}

/// `path`, relative to the descriptor `base` or the working directory.
fn resolve(process: &Process, base: Option<i32>, path: &[u8]) -> Option<PathBuf> {
    let path = Path::new(<std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(path));
    if path.is_absolute() {
        return Some(qk_input_analysis::strace::normalize(path));
    }
    let base = match base {
        None | Some(AT_FDCWD) => process.cwd.clone(),
        Some(fd) => process.descriptors.get(&fd)?.clone(),
    };
    Some(qk_input_analysis::strace::normalize(&base.join(path)))
}

/// The path an opening call opened.
fn opened(spec: &Call, entered: &Entered, process: &Process) -> Option<PathBuf> {
    let base = (spec.args.first() == Some(&Arg::AtFd)).then_some(entered.args[0] as i32);
    resolve(process, base, path_of(entered, 0)?)
}

/// What a call does to its process's descriptors and working directory.
///
/// Events come in the order calls returned, and a call can take effect
/// before another that returns first. A call given the lowest free number
/// takes it as it enters, so a close that entered while it ran freed that
/// number from its previous holder. A call that names the number, as dup2
/// does, takes it partway through, so a close of it or another dup2 onto it
/// that overlapped the call may have come before or after: the recording
/// fails rather than guess.
fn effect(spec: &Call, entered: &Entered, result: i64, process: &mut Process) -> Result<()> {
    let fd = |index: usize| entered.args[index] as i32;
    match spec.effect {
        Effect::None => {}
        Effect::Opens => {
            let path = opened(spec, entered, process);
            give(process, result as i32, path, entered, false)?;
        }
        Effect::Closes => close(process, fd(0), entered)?,
        Effect::ClosesRange => {
            const CLOSE_RANGE_CLOEXEC: u64 = 4;
            if entered.args[2] & CLOSE_RANGE_CLOEXEC == 0 {
                let (first, last) = (entered.args[0], entered.args[1]);
                let closed: std::collections::BTreeSet<i32> = process
                    .descriptors
                    .keys()
                    .chain(process.given.keys())
                    .copied()
                    .filter(|&fd| (first..=last).contains(&(fd as u32 as u64)))
                    .collect();
                for fd in closed {
                    close(process, fd, entered)?;
                }
            }
        }
        Effect::Dups => {
            let path = process.descriptors.get(&fd(0)).cloned();
            give(process, result as i32, path, entered, false)?;
        }
        // dup2 onto itself gives nothing out.
        Effect::DupsTo if fd(0) == fd(1) => {}
        Effect::DupsTo => {
            let path = process.descriptors.get(&fd(0)).cloned();
            give(process, fd(1), path, entered, true)?;
        }
        Effect::Chdir => {
            if let Some(path) = path_of(entered, 0).and_then(|path| resolve(process, None, path)) {
                process.cwd = path;
            }
        }
        Effect::Fchdir => {
            if let Some(path) = process.descriptors.get(&fd(0)).cloned() {
                process.cwd = path;
            }
        }
    }
    Ok(())
}

/// `fd` given out by `call`, as `path` where it is a file the recorder
/// knows; `named` when the call named the number.
fn give(
    process: &mut Process,
    fd: i32,
    path: Option<PathBuf>,
    call: &Entered,
    named: bool,
) -> Result<()> {
    let overlapped = if named {
        process
            .touched
            .get(&fd)
            .is_some_and(|&last| last > call.time)
    } else {
        process
            .given
            .get(&fd)
            .is_some_and(|given| given.named && given.exited > call.time)
    };
    if overlapped {
        bail!("calls that overlapped reassigned descriptor {fd}, so which came last is unknown");
    }
    match path {
        Some(path) => process.descriptors.insert(fd, path),
        None => process.descriptors.remove(&fd),
    };
    process.given.insert(
        fd,
        Given {
            entered: call.time,
            exited: call.exited,
            named,
        },
    );
    touch(process, fd, call.exited);
    Ok(())
}

/// `fd` closed by `call`, unless it was given out again after the close
/// entered, which then closed the number's previous holder.
fn close(process: &mut Process, fd: i32, call: &Entered) -> Result<()> {
    if let Some(given) = process.given.get(&fd).copied()
        && given.exited > call.time
    {
        if given.named && call.time > given.entered {
            bail!(
                "a close overlapped the call that reassigned descriptor {fd}, so which came last is unknown"
            );
        }
        touch(process, fd, call.exited);
        return Ok(());
    }
    process.descriptors.remove(&fd);
    process.given.remove(&fd);
    touch(process, fd, call.exited);
    Ok(())
}

fn touch(process: &mut Process, fd: i32, at: u64) {
    let last = process.touched.entry(fd).or_default();
    *last = (*last).max(at);
}

/// A call as strace writes it, its descriptors annotated as `-yy` does: an
/// opening call's result with `opened`, the path it resolved to, if known.
fn render(
    arch: Arch,
    spec: &Call,
    entered: &Entered,
    result: i64,
    process: &Process,
    opened: Option<&Path>,
) -> String {
    // A descriptor no recorded call opened, such as an inherited pipe or a
    // socket, is no file, as `-yy` would show; the decoder reads no path
    // from it.
    let descriptor = |fd: i32| match process.descriptors.get(&fd) {
        Some(path) => format!("{fd}<{}>", path.display()),
        None if fd >= 0 => format!("{fd}<unknown>"),
        None => fd.to_string(),
    };
    let mut paths = 0;
    let args: Vec<String> = spec
        .args
        .iter()
        .enumerate()
        .map(|(index, kind)| {
            let value = entered.args[index];
            match kind {
                Arg::Path => {
                    let text = match &entered.paths[paths] {
                        PathArg::Null => "NULL".to_owned(),
                        PathArg::Read { bytes, cut } => {
                            let quoted = quote(bytes);
                            if *cut { format!("{quoted}...") } else { quoted }
                        }
                        PathArg::Unreadable => format!("{value:#x}"),
                    };
                    paths += 1;
                    text
                }
                Arg::Fd => descriptor(value as i32),
                Arg::AtFd if value as i32 == AT_FDCWD => "AT_FDCWD".to_owned(),
                Arg::AtFd => descriptor(value as i32),
                Arg::OpenFlags => open_flags(arch, value),
                Arg::AtFlags => at_flags(value),
                Arg::Value => value.to_string(),
            }
        })
        .collect();
    let result = match opened {
        _ if result < 0 => format!("-1 {} (error)", errno(-result)),
        Some(path) => format!("{result}<{}>", path.display()),
        None => result.to_string(),
    };
    format!("{}({}) = {result}", spec.name, args.join(", "))
}

/// A path as strace quotes it, as qk's decoder unquotes it.
fn quote(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() + 2);
    text.push('"');
    for &byte in bytes {
        match byte {
            b'"' => text.push_str("\\\""),
            b'\\' => text.push_str("\\\\"),
            b'\n' => text.push_str("\\n"),
            b'\r' => text.push_str("\\r"),
            b'\t' => text.push_str("\\t"),
            0x20..=0x7e => text.push(char::from(byte)),
            _ => text.push_str(&format!("\\{byte:03o}")),
        }
    }
    text.push('"');
    text
}

fn open_flags(arch: Arch, flags: u64) -> String {
    let mut names = vec![match flags & 0o3 {
        0 => "O_RDONLY",
        1 => "O_WRONLY",
        _ => "O_RDWR",
    }];
    for (flag, name) in [
        (0o100, "O_CREAT"),
        (0o1000, "O_TRUNC"),
        (arch.o_directory(), "O_DIRECTORY"),
        (0o10000000, "O_PATH"),
        (0o2000000, "O_CLOEXEC"),
    ] {
        if flags & flag != 0 {
            names.push(name);
        }
    }
    names.join("|")
}

fn at_flags(flags: u64) -> String {
    let mut names = Vec::new();
    for (flag, name) in [
        (0x100, "AT_SYMLINK_NOFOLLOW"),
        (0x200, "AT_REMOVEDIR"),
        (0x400, "AT_SYMLINK_FOLLOW"),
        (0x1000, "AT_EMPTY_PATH"),
    ] {
        if flags & flag != 0 {
            names.push(name);
        }
    }
    if names.is_empty() {
        "0".to_owned()
    } else {
        names.join("|")
    }
}

/// An error number's name, as strace writes it.
fn errno(number: i64) -> String {
    let name = match number {
        1 => "EPERM",
        2 => "ENOENT",
        3 => "ESRCH",
        4 => "EINTR",
        5 => "EIO",
        9 => "EBADF",
        11 => "EAGAIN",
        12 => "ENOMEM",
        13 => "EACCES",
        14 => "EFAULT",
        16 => "EBUSY",
        17 => "EEXIST",
        18 => "EXDEV",
        20 => "ENOTDIR",
        21 => "EISDIR",
        22 => "EINVAL",
        24 => "EMFILE",
        26 => "ETXTBSY",
        27 => "EFBIG",
        28 => "ENOSPC",
        30 => "EROFS",
        31 => "EMLINK",
        36 => "ENAMETOOLONG",
        38 => "ENOSYS",
        39 => "ENOTEMPTY",
        40 => "ELOOP",
        61 => "ENODATA",
        75 => "EOVERFLOW",
        95 => "EOPNOTSUPP",
        _ => return format!("E{number}"),
    };
    name.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn number(name: &str) -> u16 {
        let call = crate::calls::CALLS
            .iter()
            .find(|call| call.name == name)
            .unwrap();
        Arch::X86.number(call).unwrap()
    }

    fn path(text: &str) -> PathArg {
        PathArg::Read {
            bytes: text.as_bytes().to_vec(),
            cut: false,
        }
    }

    /// A call, a second after the last.
    fn call(
        trace: &mut Trace,
        tgid: u32,
        name: &str,
        args: [u64; 6],
        paths: [PathArg; 2],
        result: i64,
    ) {
        thread_local!(static TIME: std::cell::Cell<u64> = const { std::cell::Cell::new(0) });
        let time = TIME.with(|time| {
            time.set(time.get() + 1_000_000_000);
            time.get()
        });
        call_at(trace, tgid, name, args, paths, result, [time, time + 1000]);
    }

    /// A call that entered and returned at `times`.
    fn call_at(
        trace: &mut Trace,
        tgid: u32,
        name: &str,
        args: [u64; 6],
        paths: [PathArg; 2],
        result: i64,
        [time, exited]: [u64; 2],
    ) {
        trace
            .apply(Event::Call {
                group: 1,
                tgid,
                time,
                exited,
                call: number(name),
                args,
                paths,
                result,
            })
            .unwrap();
    }

    /// What qk's decoder makes of a recorded trace.
    fn accesses(directory: &Path) -> Vec<(String, String, String)> {
        let (accesses, diagnostics) =
            qk_input_analysis::strace::collect(directory, Path::new("/work/app")).unwrap();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        accesses
            .into_iter()
            .map(|access| {
                (
                    format!("{:?}", access.operation),
                    access.path,
                    access.result,
                )
            })
            .collect()
    }

    #[test]
    fn an_exited_process_leaves_its_trace_and_its_id_to_the_next() {
        let directory = tempfile::tempdir().unwrap();
        let mut trace = Trace::new(
            Arch::X86,
            directory.path(),
            "command-0",
            Path::new("/work/app"),
            false,
        );
        trace
            .apply(Event::Begin {
                group: 1,
                tgid: 10,
                time: 0,
            })
            .unwrap();
        let at = i64::from(AT_FDCWD) as u64;
        for (name, time) in [("a.txt", 1), ("b.txt", 3)] {
            trace
                .apply(Event::Fork {
                    group: 1,
                    parent: 10,
                    child: 11,
                    time: time * 1_000_000_000,
                })
                .unwrap();
            call(
                &mut trace,
                11,
                "openat",
                [at, 0, 0, 0, 0, 0],
                [path(name), PathArg::Null],
                3,
            );
            trace
                .apply(Event::Exit {
                    group: 1,
                    tgid: 11,
                    time: (time + 1) * 1_000_000_000,
                })
                .unwrap();
            assert!(!trace.processes.contains_key(&11));
        }
        trace.finish().unwrap();
        assert_eq!(
            accesses(directory.path()),
            [
                (
                    "OpenRead".into(),
                    "/work/app/a.txt".into(),
                    "success".into()
                ),
                (
                    "OpenRead".into(),
                    "/work/app/b.txt".into(),
                    "success".into()
                ),
            ]
        );
    }

    #[test]
    fn reads_a_call_as_the_kernel_program_lays_it_out() {
        let mut bytes = vec![CALL, 0];
        bytes.extend_from_slice(&257u16.to_le_bytes());
        bytes.extend_from_slice(&7u32.to_le_bytes());
        bytes.extend_from_slice(&10u32.to_le_bytes());
        bytes.extend_from_slice(&11u32.to_le_bytes());
        bytes.extend_from_slice(&100u64.to_le_bytes());
        for arg in 1..=6u64 {
            bytes.extend_from_slice(&arg.to_le_bytes());
        }
        bytes.extend_from_slice(&(-2i64).to_le_bytes());
        bytes.extend_from_slice(&200u64.to_le_bytes());
        bytes.extend_from_slice(&4u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(b"a/b\0\0");
        assert_eq!(
            Event::parse(&bytes).unwrap(),
            Event::Call {
                group: 7,
                tgid: 10,
                time: 100,
                exited: 200,
                call: 257,
                args: [1, 2, 3, 4, 5, 6],
                paths: [path("a/b"), path("")],
                result: -2,
            }
        );
    }

    #[test]
    fn a_close_seen_after_its_number_was_given_out_again_leaves_the_new_descriptor() {
        let directory = tempfile::tempdir().unwrap();
        let mut trace = Trace::new(
            Arch::X86,
            directory.path(),
            "command-0",
            Path::new("/work/app"),
            false,
        );
        trace
            .apply(Event::Begin {
                group: 1,
                tgid: 10,
                time: 0,
            })
            .unwrap();
        let at = i64::from(AT_FDCWD) as u64;
        let second = 1_000_000_000;
        let none = || [PathArg::Null, PathArg::Null];
        call_at(
            &mut trace,
            10,
            "openat",
            [at, 0, 0o200000, 0, 0, 0],
            [path("a"), PathArg::Null],
            21,
            [second, second + 100],
        );
        // One thread closes `a` while another opens `b`, given the number
        // `a` freed, and returns first.
        call_at(
            &mut trace,
            10,
            "openat",
            [at, 0, 0o200000, 0, 0, 0],
            [path("b"), PathArg::Null],
            21,
            [3 * second, 3 * second + 100],
        );
        call_at(
            &mut trace,
            10,
            "close",
            [21, 0, 0, 0, 0, 0],
            none(),
            0,
            [2 * second, 4 * second],
        );
        call_at(
            &mut trace,
            10,
            "getdents64",
            [21, 0, 0, 0, 0, 0],
            none(),
            48,
            [5 * second, 5 * second + 100],
        );
        // Closed after it was given out, `b` is gone.
        call_at(
            &mut trace,
            10,
            "close",
            [21, 0, 0, 0, 0, 0],
            none(),
            0,
            [6 * second, 6 * second + 100],
        );
        assert!(!trace.processes[&10].descriptors.contains_key(&21));
        trace.finish().unwrap();
        assert_eq!(
            accesses(directory.path()),
            [
                ("OpenRead".into(), "/work/app/a".into(), "success".into()),
                ("OpenRead".into(), "/work/app/b".into(), "success".into()),
                (
                    "ListDirectory".into(),
                    "/work/app/b".into(),
                    "success".into()
                ),
            ]
        );
    }

    #[test]
    fn dup2_onto_itself_leaves_a_close_seen_after_it_to_close() {
        let directory = tempfile::tempdir().unwrap();
        let mut trace = Trace::new(
            Arch::X86,
            directory.path(),
            "command-0",
            Path::new("/work/app"),
            false,
        );
        let at = i64::from(AT_FDCWD) as u64;
        let second = 1_000_000_000;
        let none = || [PathArg::Null, PathArg::Null];
        call_at(
            &mut trace,
            10,
            "openat",
            [at, 0, 0o200000, 0, 0, 0],
            [path("a"), PathArg::Null],
            21,
            [second, second + 100],
        );
        call_at(
            &mut trace,
            10,
            "dup2",
            [21, 21, 0, 0, 0, 0],
            none(),
            21,
            [3 * second, 3 * second + 100],
        );
        call_at(
            &mut trace,
            10,
            "close",
            [21, 0, 0, 0, 0, 0],
            none(),
            0,
            [2 * second, 4 * second],
        );
        assert!(!trace.processes[&10].descriptors.contains_key(&21));
    }

    /// A trace whose process 10 holds `a` at 4 and `b` at 5, for calls
    /// onto 7 to race on.
    fn racing_on_seven() -> (tempfile::TempDir, Trace) {
        let directory = tempfile::tempdir().unwrap();
        let mut trace = Trace::new(
            Arch::X86,
            directory.path(),
            "command-0",
            Path::new("/work/app"),
            false,
        );
        let at = i64::from(AT_FDCWD) as u64;
        for (name, fd, time) in [("a", 4, 1), ("b", 5, 2)] {
            call_at(
                &mut trace,
                10,
                "openat",
                [at, 0, 0, 0, 0, 0],
                [path(name), PathArg::Null],
                fd,
                [time * 1_000_000_000, time * 1_000_000_000 + 100],
            );
        }
        (directory, trace)
    }

    fn try_call(
        trace: &mut Trace,
        name: &str,
        args: [u64; 6],
        result: i64,
        [time, exited]: [u64; 2],
    ) -> Result<()> {
        trace.apply(Event::Call {
            group: 1,
            tgid: 10,
            time,
            exited,
            call: number(name),
            args,
            paths: [PathArg::Null, PathArg::Null],
            result,
        })
    }

    #[test]
    fn dup2_calls_onto_one_number_that_overlapped_fail_the_recording() {
        let (_directory, mut trace) = racing_on_seven();
        let second = 1_000_000_000;
        // dup2(4, 7) enters first and returns last; dup2(5, 7) runs within.
        try_call(
            &mut trace,
            "dup2",
            [5, 7, 0, 0, 0, 0],
            7,
            [11 * second, 12 * second],
        )
        .unwrap();
        let error = try_call(
            &mut trace,
            "dup2",
            [4, 7, 0, 0, 0, 0],
            7,
            [10 * second, 13 * second],
        )
        .unwrap_err();
        assert!(format!("{error}").contains("descriptor 7"), "{error}");
    }

    #[test]
    fn a_close_that_overlapped_a_dup2_onto_its_number_fails_the_recording() {
        let (_directory, mut trace) = racing_on_seven();
        let second = 1_000_000_000;
        try_call(
            &mut trace,
            "dup2",
            [4, 7, 0, 0, 0, 0],
            7,
            [10 * second, 12 * second],
        )
        .unwrap();
        let error = try_call(
            &mut trace,
            "close",
            [7, 0, 0, 0, 0, 0],
            0,
            [11 * second, 13 * second],
        )
        .unwrap_err();
        assert!(format!("{error}").contains("descriptor 7"), "{error}");
    }

    #[test]
    fn calls_on_one_number_one_after_another_follow_each_other() {
        let (_directory, mut trace) = racing_on_seven();
        let second = 1_000_000_000;
        try_call(
            &mut trace,
            "dup2",
            [4, 7, 0, 0, 0, 0],
            7,
            [10 * second, 11 * second],
        )
        .unwrap();
        try_call(
            &mut trace,
            "dup2",
            [5, 7, 0, 0, 0, 0],
            7,
            [12 * second, 13 * second],
        )
        .unwrap();
        assert_eq!(
            trace.processes[&10].descriptors[&7],
            Path::new("/work/app/b")
        );
        try_call(
            &mut trace,
            "close",
            [7, 0, 0, 0, 0, 0],
            0,
            [14 * second, 15 * second],
        )
        .unwrap();
        assert!(!trace.processes[&10].descriptors.contains_key(&7));
    }

    #[test]
    fn descriptors_no_recorded_call_opened_name_no_file() {
        let directory = tempfile::tempdir().unwrap();
        let mut trace = Trace::new(
            Arch::X86,
            directory.path(),
            "command-0",
            Path::new("/work/app"),
            false,
        );
        trace
            .apply(Event::Begin {
                group: 1,
                tgid: 10,
                time: 0,
            })
            .unwrap();
        let at = i64::from(AT_FDCWD) as u64;
        const AT_EMPTY_PATH: u64 = 0x1000;
        let empty = || [path(""), PathArg::Null];
        // Inherited standard output, which the shell looks at.
        call(
            &mut trace,
            10,
            "newfstatat",
            [1, 0, 0, AT_EMPTY_PATH, 0, 0],
            empty(),
            0,
        );
        // A file opened, then closed with every descriptor above it; the
        // pipe later given its number is no longer the file.
        call(
            &mut trace,
            10,
            "openat",
            [at, 0, 0, 0, 0, 0],
            [path("a.txt"), PathArg::Null],
            3,
        );
        call(
            &mut trace,
            10,
            "close_range",
            [3, u64::from(u32::MAX), 0, 0, 0, 0],
            [PathArg::Null, PathArg::Null],
            0,
        );
        call(
            &mut trace,
            10,
            "newfstatat",
            [3, 0, 0, AT_EMPTY_PATH, 0, 0],
            empty(),
            0,
        );
        trace.finish().unwrap();
        assert_eq!(
            accesses(directory.path()),
            [(
                "OpenRead".into(),
                "/work/app/a.txt".into(),
                "success".into()
            )]
        );
    }

    #[test]
    fn writes_calls_as_strace_does_with_descriptors_and_working_directories() {
        let directory = tempfile::tempdir().unwrap();
        let mut trace = Trace::new(
            Arch::X86,
            directory.path(),
            "command-0",
            Path::new("/work/app"),
            false,
        );
        trace
            .apply(Event::Begin {
                group: 1,
                tgid: 10,
                time: 0,
            })
            .unwrap();
        let at = i64::from(AT_FDCWD) as u64;
        // Opens a directory, lists it, and reads a file relative to it.
        call(
            &mut trace,
            10,
            "openat",
            [at, 0, 0o200000, 0, 0, 0],
            [path("src"), PathArg::Null],
            3,
        );
        call(
            &mut trace,
            10,
            "getdents64",
            [3, 0, 0, 0, 0, 0],
            [PathArg::Null, PathArg::Null],
            48,
        );
        call(
            &mut trace,
            10,
            "openat",
            [3, 0, 0, 0, 0, 0],
            [path("main.ts"), PathArg::Null],
            4,
        );
        // Misses a file, changes directory into the one it opened, and forks
        // a child that reads relative to it.
        call(
            &mut trace,
            10,
            "newfstatat",
            [at, 0, 0, 0, 0, 0],
            [path("missing.ts"), PathArg::Null],
            -2,
        );
        call(
            &mut trace,
            10,
            "fchdir",
            [3, 0, 0, 0, 0, 0],
            [PathArg::Null, PathArg::Null],
            0,
        );
        // After the calls above, which the helper times a second apart.
        trace
            .apply(Event::Fork {
                group: 1,
                parent: 10,
                child: 11,
                time: 5_500_000_000,
            })
            .unwrap();
        call(
            &mut trace,
            11,
            "openat",
            [at, 0, 0o1 | 0o100, 0, 0, 0],
            [path("out.js"), PathArg::Null],
            3,
        );
        trace.finish().unwrap();
        assert_eq!(
            accesses(directory.path()),
            [
                ("OpenRead".into(), "/work/app/src".into(), "success".into()),
                (
                    "ListDirectory".into(),
                    "/work/app/src".into(),
                    "success".into()
                ),
                (
                    "OpenRead".into(),
                    "/work/app/src/main.ts".into(),
                    "success".into()
                ),
                (
                    "Metadata".into(),
                    "/work/app/missing.ts".into(),
                    "missing".into()
                ),
                (
                    "Write".into(),
                    "/work/app/src/out.js".into(),
                    "success".into()
                ),
            ]
        );
    }

    #[test]
    fn resolves_an_opened_path_through_its_links_when_asked() {
        let directory = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(directory.path()).unwrap();
        std::fs::create_dir(root.join("real")).unwrap();
        std::fs::write(root.join("real/file"), "x").unwrap();
        std::os::unix::fs::symlink("real", root.join("link")).unwrap();
        let traces = root.join("traces");
        std::fs::create_dir(&traces).unwrap();
        let mut trace = Trace::new(Arch::X86, &traces, "command-0", &root, true);
        trace
            .apply(Event::Begin {
                group: 1,
                tgid: 10,
                time: 0,
            })
            .unwrap();
        let at = i64::from(AT_FDCWD) as u64;
        call(
            &mut trace,
            10,
            "openat",
            [at, 0, 0, 0, 0, 0],
            [path("link/file"), PathArg::Null],
            3,
        );
        trace.finish().unwrap();
        let (accesses, _) = qk_input_analysis::strace::collect(&traces, &root).unwrap();
        assert_eq!(
            accesses[0].path,
            root.join("link/file").display().to_string()
        );
        assert_eq!(
            accesses[0].resolved_path.as_deref(),
            Some(root.join("real/file").display().to_string().as_str())
        );
    }

    #[test]
    fn a_path_it_could_not_read_whole_leaves_its_call_undecoded() {
        let directory = tempfile::tempdir().unwrap();
        let mut trace = Trace::new(
            Arch::X86,
            directory.path(),
            "command-0",
            Path::new("/work"),
            false,
        );
        trace
            .apply(Event::Begin {
                group: 1,
                tgid: 10,
                time: 0,
            })
            .unwrap();
        let cut = PathArg::Read {
            bytes: b"/work/long".to_vec(),
            cut: true,
        };
        call(&mut trace, 10, "stat", [0; 6], [cut, PathArg::Null], 0);
        call(
            &mut trace,
            10,
            "stat",
            [0x1234, 0, 0, 0, 0, 0],
            [PathArg::Unreadable, PathArg::Null],
            0,
        );
        trace.finish().unwrap();
        let (_, diagnostics) =
            qk_input_analysis::strace::collect(directory.path(), Path::new("/work")).unwrap();
        assert!(
            diagnostics
                .iter()
                .any(|line| line.starts_with("2 syscall records could not be decoded")),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn quotes_paths_as_the_decoder_reads_them() {
        assert_eq!(quote(b"a b\"c\\d\n\x01"), "\"a b\\\"c\\\\d\\n\\001\"");
    }
}
