//! Recording what a task's commands do to files, with eBPF or strace.

use std::path::{Path, PathBuf};

use anyhow::Context;
use qk_input_analysis::Access;
use qk_input_analysis::strace::Limits;

/// The recorder `QK_RECORDER` names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    /// eBPF where its helper can load it, else strace: unset or `auto`.
    Auto,
    Ebpf,
    Strace,
}

impl Choice {
    pub fn from_env() -> anyhow::Result<Self> {
        Self::set(std::env::var_os("QK_RECORDER").as_deref())
    }

    /// The choice `QK_RECORDER`'s value makes; unset, `auto`.
    fn set(value: Option<&std::ffi::OsStr>) -> anyhow::Result<Self> {
        match value.map(std::ffi::OsStr::to_str) {
            None => Ok(Self::Auto),
            Some(Some(name)) => Self::named(name),
            Some(None) => anyhow::bail!(
                "unknown QK_RECORDER {value:?}, which is not UTF-8; use auto, ebpf or strace",
                value = value.unwrap_or_default()
            ),
        }
    }

    fn named(name: &str) -> anyhow::Result<Self> {
        match name {
            "" | "auto" => Ok(Self::Auto),
            "ebpf" => Ok(Self::Ebpf),
            "strace" => Ok(Self::Strace),
            other => anyhow::bail!("unknown QK_RECORDER {other:?}; use auto, ebpf or strace"),
        }
    }
}

/// Whether a recorder can record here.
#[derive(Debug)]
pub enum Probed<R = Recording> {
    Ready(R),
    /// Nothing records on this system.
    Unsupported,
    /// No recorder works here, and none was named: why the last one tried
    /// cannot record.
    Unavailable(anyhow::Error),
}

/// The recorder `choice` gives, of eBPF's and strace's: one named must
/// record, and `auto` takes eBPF where it can, else strace. Only Linux
/// records; elsewhere `auto` records nothing, and a recorder named fails.
fn decide<R>(
    choice: Choice,
    linux: bool,
    ebpf: impl FnOnce() -> Result<R, String>,
    strace: impl FnOnce() -> anyhow::Result<R>,
) -> anyhow::Result<Probed<R>> {
    if !linux {
        return match choice {
            Choice::Auto => Ok(Probed::Unsupported),
            Choice::Ebpf => anyhow::bail!(
                "recording with eBPF needs Linux; unset QK_RECORDER or set it to auto"
            ),
            Choice::Strace => anyhow::bail!(
                "recording with strace needs Linux; unset QK_RECORDER or set it to auto"
            ),
        };
    }
    match choice {
        Choice::Ebpf => match ebpf() {
            Ok(recording) => Ok(Probed::Ready(recording)),
            Err(error) => anyhow::bail!("cannot record with eBPF: {error}"),
        },
        Choice::Strace => strace().map(Probed::Ready),
        Choice::Auto => match ebpf() {
            Ok(recording) => Ok(Probed::Ready(recording)),
            Err(_) => Ok(match strace() {
                Ok(recording) => Probed::Ready(recording),
                Err(error) => Probed::Unavailable(error),
            }),
        },
    }
}

/// An external syscall recorder wrapping each command, with separate trace files.
#[derive(Clone, Debug)]
pub struct Recording {
    /// The recorder's program: strace, or qk itself, whose helper loads the
    /// eBPF recorder.
    pub program: PathBuf,
    pub directory: PathBuf,
    /// Whether strace filters the traced calls in the kernel, with seccomp,
    /// so untraced calls run at full speed. strace 5.3 and later can.
    pub seccomp: bool,
    /// Whether it may key the task's result for `qk:ultracache`, as
    /// `--record` gives it. A recording made only to audit the task does not.
    keys: bool,
    /// The eBPF recorder, and the recordings of this task's commands it has
    /// begun; `None` when strace records.
    #[cfg(target_os = "linux")]
    ebpf: Option<Ebpf>,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Debug)]
struct Ebpf {
    recorder: std::sync::Arc<qk_trace::Recorder>,
    sessions: std::sync::Arc<std::sync::Mutex<Vec<qk_trace::Session>>>,
    /// Why settling failed, kept so that a later settle, as the cache's and
    /// then an audit's of one recording, does not find it whole.
    failure: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    /// Whether an opened file's path is recorded with what its links lead
    /// to, as strace records it, which costs a lookup an open.
    resolve: bool,
}

/// The eBPF recorder, its helper started once a process: as qk itself when
/// qk runs as root, else through `sudo -n`, so that it is never asked for a
/// password.
#[cfg(target_os = "linux")]
fn ebpf_recorder() -> Result<std::sync::Arc<qk_trace::Recorder>, String> {
    static RECORDER: std::sync::OnceLock<Result<std::sync::Arc<qk_trace::Recorder>, String>> =
        std::sync::OnceLock::new();
    RECORDER
        .get_or_init(|| {
            let qk = std::env::current_exe().map_err(|error| error.to_string())?;
            // SAFETY: geteuid has no preconditions.
            let helper = if unsafe { libc::geteuid() } == 0 {
                let mut helper = std::process::Command::new(qk);
                helper.arg("__trace-helper");
                helper
            } else {
                let mut helper = std::process::Command::new("sudo");
                helper.args(["-n", "--"]).arg(qk).arg("__trace-helper");
                helper
            };
            qk_trace::Recorder::start(helper).map_err(|error| format!("{error:#}"))
        })
        .clone()
}

impl Recording {
    /// The recorder `QK_RECORDER` picks: by default eBPF, where its helper
    /// can load it, else strace on `PATH`, checked to record child
    /// processes, with seccomp filtering where it supports it. Its trial
    /// traces go in `directory`. Err: `QK_RECORDER` misnamed, or the
    /// recorder it names cannot record.
    pub fn probe(directory: &Path) -> anyhow::Result<Probed> {
        decide(
            Choice::from_env()?,
            cfg!(target_os = "linux"),
            || Self::ebpf(directory),
            || Self::strace(directory),
        )
    }

    /// The recorder [`Self::probe`] picks, for what cannot go on without
    /// one: the sandbox's audit, input analysis.
    pub fn required(directory: &Path) -> anyhow::Result<Self> {
        match Self::probe(directory)? {
            Probed::Ready(recording) => Ok(recording),
            Probed::Unavailable(error) => Err(error),
            Probed::Unsupported => anyhow::bail!("recording needs Linux"),
        }
    }

    #[cfg(target_os = "linux")]
    fn ebpf(directory: &Path) -> Result<Self, String> {
        let recorder = ebpf_recorder()?;
        Ok(Self {
            program: std::env::current_exe().map_err(|error| error.to_string())?,
            directory: directory.to_owned(),
            seccomp: false,
            keys: false,
            ebpf: Some(Ebpf {
                recorder,
                sessions: Default::default(),
                failure: Default::default(),
                resolve: false,
            }),
        })
    }

    #[cfg(not(target_os = "linux"))]
    fn ebpf(_: &Path) -> Result<Self, String> {
        Err("the eBPF recorder records only on Linux".into())
    }

    fn strace(directory: &Path) -> anyhow::Result<Self> {
        let path = std::env::var_os("PATH").context("recording needs strace on PATH")?;
        let program = std::env::split_paths(&path)
            .map(|directory| directory.join("strace"))
            .find(|candidate| candidate.is_file())
            .context("recording needs strace on PATH; install strace and retry")?
            .canonicalize()?;
        let mut failure = String::new();
        for seccomp in [true, false] {
            let recording = Self {
                program: program.clone(),
                directory: directory.to_owned(),
                seccomp,
                keys: false,
                #[cfg(target_os = "linux")]
                ebpf: None,
            };
            let probe = recording
                .command(0, "exec /bin/true", Path::new("/"))
                .output()
                .context("cannot probe strace")?;
            if probe.status.success() {
                return Ok(recording);
            }
            failure = String::from_utf8_lossy(&probe.stderr).into_owned();
        }
        anyhow::bail!("strace cannot record child processes: {failure}")
    }

    /// The recorder's name, as reports give it: `ebpf` or `strace`.
    pub fn backend(&self) -> &'static str {
        #[cfg(target_os = "linux")]
        if self.ebpf.is_some() {
            return "ebpf";
        }
        "strace"
    }

    /// The same settings, with traces in `directory`.
    pub fn in_directory(&self, directory: PathBuf) -> Self {
        Self {
            directory,
            #[cfg(target_os = "linux")]
            ebpf: self.ebpf.as_ref().map(|ebpf| Ebpf {
                recorder: ebpf.recorder.clone(),
                sessions: Default::default(),
                failure: Default::default(),
                resolve: ebpf.resolve,
            }),
            ..self.clone()
        }
    }

    /// The same, recording an opened file's path with what its links lead
    /// to, as a report of what a task read shows it. strace always does.
    pub fn resolving(self) -> Self {
        #[cfg(target_os = "linux")]
        {
            let mut recording = self;
            if let Some(ebpf) = &mut recording.ebpf {
                ebpf.resolve = true;
            }
            recording
        }
        #[cfg(not(target_os = "linux"))]
        self
    }

    /// The same, keying the task's result: see [`Self::keys`].
    pub fn keying(self) -> Self {
        Self { keys: true, ..self }
    }

    /// Whether it may key the task's result for `qk:ultracache`, as
    /// `--record` gives it. A recording made only to audit the task does not.
    pub fn keys(&self) -> bool {
        self.keys
    }

    /// Ends the recordings of the commands that have run, once they have
    /// finished, so that their traces are whole. strace's are as soon as it
    /// exits; the eBPF recorder's once its helper has sent every event.
    pub fn settle(&self) -> anyhow::Result<()> {
        #[cfg(target_os = "linux")]
        if let Some(ebpf) = &self.ebpf {
            if let Some(failure) = ebpf.failure.lock().unwrap().as_ref() {
                anyhow::bail!("{failure}");
            }
            let sessions = std::mem::take(&mut *ebpf.sessions.lock().unwrap());
            let mut sessions = sessions.into_iter();
            while let Some(session) = sessions.next() {
                if let Err(error) = ebpf.recorder.finish(session) {
                    *ebpf.failure.lock().unwrap() = Some(format!("{error:#}"));
                    // The rest without waiting again: a helper that did not
                    // answer once will not now.
                    sessions
                        .by_ref()
                        .for_each(|rest| ebpf.recorder.abandon(rest));
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    /// What the commands that have run did, settled, within `limits`, with
    /// what kept it from being complete; their paths relative to `cwd`,
    /// where they started.
    pub fn accesses(
        &self,
        cwd: &Path,
        limits: &Limits,
    ) -> anyhow::Result<(Vec<Access>, Vec<String>)> {
        self.settle()?;
        qk_input_analysis::strace::collect_within(&self.directory, cwd, limits)
    }

    /// The same, whole: what kept it from being complete, the first issue,
    /// is the error.
    pub fn whole_accesses(&self, cwd: &Path, limits: &Limits) -> Result<Vec<Access>, String> {
        let (accesses, issues) = self
            .accesses(cwd, limits)
            .map_err(|error| format!("{error:#}"))?;
        match issues.into_iter().next() {
            Some(issue) => Err(issue),
            None => Ok(accesses),
        }
    }

    /// The same invocation is used by preflight and actual task commands,
    /// which start in `cwd`.
    pub fn command(&self, index: usize, text: &str, cwd: &Path) -> std::process::Command {
        #[cfg(target_os = "linux")]
        if let Some(ebpf) = &self.ebpf {
            use std::os::unix::process::CommandExt;
            let session = ebpf.recorder.begin(
                &self.directory,
                &format!("command-{index}"),
                cwd,
                ebpf.resolve,
            );
            ebpf.sessions.lock().unwrap().push(session);
            let mut command = crate::process::shell(text);
            // SAFETY: the hook makes one system call.
            unsafe { command.pre_exec(ebpf.recorder.join(session)) };
            return command;
        }
        let _ = cwd;
        let mut command = std::process::Command::new(&self.program);
        // No read/write buffers or exec argument/environment arrays.
        if self.seccomp {
            command.arg("--seccomp-bpf");
        }
        command
            .args([
                "-ff",
                "-ttt",
                "-yy",
                "-qq",
                "-s",
                "4096",
                "-e",
                "trace=%file,%process,getdents64,fchdir,close,dup,dup2,dup3,getcwd",
                "-e",
                "raw=execve,execveat",
                "-o",
            ])
            .arg(self.directory.join(format!("command-{index}")))
            .args(["--", "/bin/sh", "-c", text]);
        command
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decided(
        choice: Choice,
        ebpf: Result<&'static str, &'static str>,
        strace: Result<&'static str, &'static str>,
    ) -> Result<Probed<&'static str>, String> {
        decide(
            choice,
            true,
            || ebpf.map_err(str::to_owned),
            || strace.map_err(|error| anyhow::anyhow!(error)),
        )
        .map_err(|error| format!("{error:#}"))
    }

    fn ready(probed: Result<Probed<&'static str>, String>) -> Option<&'static str> {
        match probed {
            Ok(Probed::Ready(recording)) => Some(recording),
            _ => None,
        }
    }

    #[test]
    fn auto_takes_ebpf_then_strace_and_goes_on_without_either() {
        assert_eq!(
            ready(decided(Choice::Auto, Ok("ebpf"), Ok("strace"))),
            Some("ebpf")
        );
        assert_eq!(
            ready(decided(Choice::Auto, Err("no"), Ok("strace"))),
            Some("strace")
        );
        match decided(Choice::Auto, Err("no eBPF"), Err("no strace")) {
            Ok(Probed::Unavailable(error)) => assert_eq!(error.to_string(), "no strace"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_named_recorder_records_or_fails() {
        assert_eq!(
            ready(decided(Choice::Ebpf, Ok("ebpf"), Ok("strace"))),
            Some("ebpf")
        );
        assert_eq!(
            decided(Choice::Ebpf, Err("no helper"), Ok("strace")).unwrap_err(),
            "cannot record with eBPF: no helper"
        );
        assert_eq!(
            ready(decided(Choice::Strace, Ok("ebpf"), Ok("strace"))),
            Some("strace")
        );
        assert_eq!(
            decided(Choice::Strace, Ok("ebpf"), Err("no strace")).unwrap_err(),
            "no strace"
        );
    }

    #[test]
    fn strace_named_is_never_ebpf() {
        let probed = decide(
            Choice::Strace,
            true,
            || -> Result<&str, String> { panic!("eBPF probed") },
            || Ok("strace"),
        );
        assert_eq!(
            ready(probed.map_err(|error| error.to_string())),
            Some("strace")
        );
    }

    #[test]
    fn off_linux_auto_records_nothing_and_a_recorder_named_fails() {
        let off = |choice| {
            decide(
                choice,
                false,
                || -> Result<&str, String> { panic!("eBPF probed") },
                || panic!("strace probed"),
            )
        };
        assert!(matches!(off(Choice::Auto), Ok(Probed::Unsupported)));
        assert_eq!(
            off(Choice::Ebpf).unwrap_err().to_string(),
            "recording with eBPF needs Linux; unset QK_RECORDER or set it to auto"
        );
        assert_eq!(
            off(Choice::Strace).unwrap_err().to_string(),
            "recording with strace needs Linux; unset QK_RECORDER or set it to auto"
        );
    }

    #[cfg(unix)]
    #[test]
    fn qk_recorder_unset_is_auto_and_not_utf8_fails() {
        use std::os::unix::ffi::OsStrExt;
        assert_eq!(Choice::set(None).unwrap(), Choice::Auto);
        assert_eq!(
            Choice::set(Some("strace".as_ref())).unwrap(),
            Choice::Strace
        );
        let error = Choice::set(Some(std::ffi::OsStr::from_bytes(b"str\xffce")))
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("unknown QK_RECORDER "), "{error}");
        assert!(
            error.ends_with(", which is not UTF-8; use auto, ebpf or strace"),
            "{error}"
        );
    }

    #[test]
    fn qk_recorder_names_one_of_three() {
        assert_eq!(Choice::named("").unwrap(), Choice::Auto);
        assert_eq!(Choice::named("auto").unwrap(), Choice::Auto);
        assert_eq!(Choice::named("ebpf").unwrap(), Choice::Ebpf);
        assert_eq!(Choice::named("strace").unwrap(), Choice::Strace);
        assert_eq!(
            Choice::named("strase").unwrap_err().to_string(),
            "unknown QK_RECORDER \"strase\"; use auto, ebpf or strace"
        );
    }
}
