use std::collections::BTreeSet;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use command_group::{CommandGroup, GroupChild};

use crate::{Capture, Decoration, Display, PreparedTask, Shown};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Success,
    Failed(i32),
    Cancelled,
}

/// A running command: in a process group of its own or, under a
/// pseudo-terminal, a session of its own.
enum Running {
    Group(GroupChild),
    #[cfg(unix)]
    Session(crate::pty::Session),
}

impl Running {
    fn id(&self) -> u32 {
        match self {
            Self::Group(child) => child.id(),
            #[cfg(unix)]
            Self::Session(child) => child.id(),
        }
    }

    fn inner(&mut self) -> &mut std::process::Child {
        match self {
            Self::Group(child) => child.inner(),
            #[cfg(unix)]
            Self::Session(child) => child.inner(),
        }
    }

    fn kill(&mut self) -> std::io::Result<()> {
        match self {
            Self::Group(child) => child.kill(),
            #[cfg(unix)]
            Self::Session(child) => child.kill(),
        }
    }

    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        match self {
            Self::Group(child) => child.try_wait(),
            #[cfg(unix)]
            Self::Session(child) => child.try_wait(),
        }
    }

    fn wait(&mut self) -> std::io::Result<ExitStatus> {
        match self {
            Self::Group(child) => child.wait(),
            #[cfg(unix)]
            Self::Session(child) => child.wait(),
        }
    }

    #[cfg(unix)]
    fn terminate(&self) {
        use command_group::{Signal, UnixChildExt};
        let _ = match self {
            Self::Group(child) => child.signal(Signal::SIGTERM),
            Self::Session(child) => child.signal(libc::SIGTERM),
        };
    }
}

/// The running commands, whose ids the task publishes while they run.
struct Children<'a>(Vec<Running>, &'a Mutex<BTreeSet<u32>>);

impl Children<'_> {
    fn push(&mut self, child: Running) {
        self.1.lock().unwrap().insert(child.id());
        self.0.push(child);
    }

    fn remove(&mut self, index: usize) -> Running {
        let child = self.0.swap_remove(index);
        self.1.lock().unwrap().remove(&child.id());
        child
    }
}

impl Drop for Children<'_> {
    fn drop(&mut self) {
        // Also clean up when spawning or polling one command fails.
        for child in &mut self.0 {
            let _ = child.kill();
        }
        for child in &mut self.0 {
            let _ = child.wait();
        }
        let mut ids = self.1.lock().unwrap();
        for child in &self.0 {
            ids.remove(&child.id());
        }
    }
}

/// How long a cancelled or stopped command may take to exit after SIGTERM:
/// NX_PROCESS_KILL_GRACE_PERIOD milliseconds, 5000 by default, as in Nx.
#[cfg(unix)]
fn grace() -> Duration {
    grace_period(
        std::env::var("NX_PROCESS_KILL_GRACE_PERIOD")
            .ok()
            .as_deref(),
    )
}

#[cfg(unix)]
fn grace_period(value: Option<&str>) -> Duration {
    Duration::from_millis(
        value
            .and_then(|value| value.parse::<u32>().ok())
            .map_or(5000, u64::from),
    )
}

impl Children<'_> {
    /// Asks every process group to exit, then leaves stragglers to `Drop`.
    fn terminate(&mut self) {
        #[cfg(unix)]
        {
            for child in &self.0 {
                child.terminate();
            }
            let deadline = std::time::Instant::now() + grace();
            while std::time::Instant::now() < deadline
                && self
                    .0
                    .iter_mut()
                    .any(|child| matches!(child.try_wait(), Ok(None)))
            {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

pub fn execute(task: &PreparedTask, cancelled: &AtomicBool) -> Result<Outcome> {
    let log_matches = match &task.ready_check {
        Some(Ok(crate::readiness::ReadyCheck {
            probe: crate::readiness::Probe::LogMatches(patterns),
            ..
        })) => Some(patterns),
        _ => None,
    };
    if task.display == Display::Stream
        && task.ready_when.is_empty()
        && log_matches.is_none()
        && task.decorations.iter().all(Decoration::is_empty)
    {
        // Straight through: the task writes to the terminal itself.
        return execute_captured(task, cancelled, None);
    }
    let mut capture =
        Capture::new(None, task.display.clone()).ready_when(&task.ready_when, task.ready.clone());
    if let Some(patterns) = log_matches {
        capture = capture.log_matches(patterns, task.verdict.clone());
    }
    execute_captured(task, cancelled, Some(&capture))
}

pub fn execute_captured(
    task: &PreparedTask,
    cancelled: &AtomicBool,
    capture: Option<&Capture>,
) -> Result<Outcome> {
    // Takes the terminal back from an interactive command, however it ends.
    let _terminal = (task.interactive && task.commands.len() == 1)
        .then(terminal::Foreground::take_back_on_drop);
    std::thread::scope(|scope| {
        let mut readers = Vec::new();
        let outcome = (|| {
            // Dropped after `children`, once the interactive command is gone.
            #[cfg(unix)]
            let mut forwarders = Vec::new();
            let mut pending = task.commands.iter().enumerate();
            let mut children = Children(Vec::new(), &task.processes);
            let mut launched = Instant::now();
            loop {
                if cancelled.load(Ordering::SeqCst) {
                    children.terminate();
                    return Ok(Outcome::Cancelled);
                }
                if children.0.is_empty() || task.parallel {
                    for (index, command) in pending.by_ref() {
                        let interactive = task.interactive && task.commands.len() == 1;
                        let (mut child, leader) =
                            spawn(task, command, index, capture.is_some(), interactive)
                                .with_context(|| format!("cannot start command for {}", task.id))?;
                        if let Some(capture) = capture {
                            let decoration = task.decorations.get(index);
                            match leader {
                                // One stream, as the terminal merges them.
                                #[cfg(unix)]
                                Some(leader) => {
                                    if interactive && crate::pty::forwards_input() {
                                        forwarders.push(crate::pty::Forwarder::start(
                                            leader.try_clone()?,
                                            task.tty,
                                        ));
                                    }
                                    let output = crate::pty::Output(leader);
                                    readers
                                        .push(scope.spawn(move || {
                                            capture.copy(output, false, decoration)
                                        }));
                                }
                                _ => {
                                    let stdout = child
                                        .inner()
                                        .stdout
                                        .take()
                                        .context("missing stdout pipe")?;
                                    let stderr = child
                                        .inner()
                                        .stderr
                                        .take()
                                        .context("missing stderr pipe")?;
                                    readers
                                        .push(scope.spawn(move || {
                                            capture.copy(stdout, false, decoration)
                                        }));
                                    readers.push(
                                        scope.spawn(move || capture.copy(stderr, true, decoration)),
                                    );
                                }
                            }
                        }
                        children.push(child);
                        launched = Instant::now();
                        if !task.parallel {
                            break;
                        }
                    }
                }
                if children.0.is_empty() {
                    return Ok(Outcome::Success);
                }
                let mut index = 0;
                while index < children.0.len() {
                    match children.0[index]
                        .try_wait()
                        .context("cannot wait for command")?
                    {
                        Some(status) => {
                            let mut child = children.remove(index);
                            // Finite tasks may not leave background descendants running.
                            let _ = child.kill();
                            if !status.success() {
                                // As in Nx, commands run side by side fail the
                                // task with 1, one after another with the code.
                                let code = if task.parallel
                                    && task.commands.len() > 1
                                    && task.ready_when.is_empty()
                                {
                                    1
                                } else {
                                    exit_code(status)
                                };
                                return Ok(Outcome::Failed(code));
                            }
                        }
                        None => index += 1,
                    }
                }
                if !children.0.is_empty() {
                    // Short commands should not pay a scheduler tick at every
                    // task boundary. Long builds retain the low polling rate.
                    let interval = if launched.elapsed() < Duration::from_millis(250) {
                        1
                    } else {
                        20
                    };
                    std::thread::sleep(Duration::from_millis(interval));
                }
            }
        })();
        for reader in readers {
            reader
                .join()
                .map_err(|_| anyhow::anyhow!("output capture thread panicked"))??;
        }
        // As in Nx, commands that all exit without printing their readyWhen
        // text fail the task: it never became ready, and nothing waiting on
        // it would have started.
        let outcome = match (capture, outcome) {
            (Some(capture), Ok(Outcome::Success)) => {
                let missing = capture.unready();
                if missing.is_empty() {
                    Ok(Outcome::Success)
                } else {
                    let exited = match task.commands.as_slice() {
                        [command] => format!("command \"{command}\""),
                        _ => "all commands".to_owned(),
                    };
                    let missing = missing
                        .iter()
                        .map(|text| format!("\"{text}\""))
                        .collect::<Vec<_>>()
                        .join(", ");
                    capture.note(&format!(
                        "Warning: {exited} exited before printing the readyWhen text {missing}"
                    ))?;
                    Ok(Outcome::Failed(1))
                }
            }
            (_, outcome) => outcome,
        };
        if let (Some(capture), Ok(outcome)) = (capture, &outcome) {
            capture.finish(if *outcome == Outcome::Success {
                Shown::Success
            } else {
                Shown::Failure
            })?;
        }
        outcome
    })
}

/// A command running `text` in the shell tasks run in: `/bin/sh` on Unix,
/// `cmd.exe` on Windows.
pub fn shell(text: &str) -> Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let mut command =
            Command::new(std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into()));
        command
            .args(["/D", "/S", "/C"])
            .raw_arg(format!("\"{text}\""));
        command
    }
    #[cfg(not(windows))]
    {
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg(text);
        command
    }
}

/// Launch a task command with its environment, confinement and output streams.
/// A captured command runs under a pseudo-terminal when qk's output is a
/// terminal, as in Nx; the terminal's leader end is returned with it.
fn spawn(
    task: &PreparedTask,
    text: &str,
    index: usize,
    capture: bool,
    interactive: bool,
) -> Result<(Running, Option<std::fs::File>)> {
    // Checked here rather than when the task is prepared: a dependency may
    // create the directory, as a native prebuild creates the project a build
    // runs in.
    if !task.cwd.is_dir() {
        anyhow::bail!("working directory does not exist: {}", task.cwd.display());
    }
    #[cfg(windows)]
    let mut command = shell(text);
    #[cfg(not(windows))]
    let mut command = match &task.sandbox {
        Some(crate::Confinement::Seatbelt(profile)) => {
            let mut command = Command::new("sandbox-exec");
            command.arg("-f").arg(profile).args(["/bin/sh", "-c", text]);
            command
        }
        _ => shell(text),
    };
    if let Some(recording) = &task.recording {
        command = recording.command(index, text, &task.cwd);
    }
    #[cfg(target_os = "linux")]
    if let Some(crate::Confinement::Landlock(ruleset)) = task.sandbox {
        use std::os::unix::process::CommandExt;
        // Between fork and exec: only system calls.
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
                    || libc::syscall(libc::SYS_landlock_restrict_self, ruleset, 0u32) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    command
        .current_dir(&task.cwd)
        .env_clear()
        .envs(&task.env)
        .envs(&task.execution)
        .stdin(if interactive {
            Stdio::inherit()
        } else {
            Stdio::null()
        })
        .stdout(if capture {
            Stdio::piped()
        } else {
            Stdio::inherit()
        })
        .stderr(if capture {
            Stdio::piped()
        } else {
            Stdio::inherit()
        });
    if let Some(node) = &task.node_path
        && let Some((key, path)) = node.path(&task.env)?
    {
        command.env(key, path);
    }
    // Re-entered exec inherits confinement. Override any caller-supplied
    // marker so an unsandboxed task cannot accidentally claim confinement.
    command.env_remove("QK_TASK_SANDBOX");
    if task.sandbox.is_some() {
        command.env("QK_TASK_SANDBOX", "1");
    }
    #[cfg(unix)]
    if capture
        && task.pseudo_terminal
        // As in Nx, prefixed lines are not drawn on a terminal.
        && task
            .decorations
            .get(index)
            .is_none_or(|decoration| decoration.prefix.is_none())
        && std::io::IsTerminal::is_terminal(&std::io::stdout())
    {
        let input = interactive && crate::pty::forwards_input();
        let (session, leader) = crate::pty::spawn(&mut command, input)?;
        return Ok((Running::Session(session), Some(leader)));
    }
    #[cfg(unix)]
    if interactive && terminal::in_foreground() {
        use std::os::unix::process::CommandExt;
        // Each command runs in its own process group, which the terminal
        // stops when it reads; so the command makes its group the terminal's
        // foreground before it starts, as a shell does for a job.
        unsafe {
            command.pre_exec(|| {
                terminal::claim();
                Ok(())
            });
        }
    }
    Ok((Running::Group(command.group_spawn()?), None))
}

/// Handing the terminal to an interactive command and back.
pub(crate) mod terminal {
    /// Whether stdin is a terminal whose foreground is qk's process group.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub fn in_foreground() -> bool {
        #[cfg(unix)]
        unsafe {
            libc::isatty(0) == 1 && libc::tcgetpgrp(0) == libc::getpgrp()
        }
        #[cfg(not(unix))]
        false
    }

    /// Makes the calling process's group the terminal's foreground. Run in
    /// the child between fork and exec, so only async-signal-safe calls.
    #[cfg(unix)]
    pub fn claim() {
        unsafe { foreground(libc::getpgrp()) }
    }

    /// `tcsetpgrp` from a background group raises SIGTTOU, which would stop
    /// the caller; it is ignored around the call.
    #[cfg(unix)]
    unsafe fn foreground(group: libc::pid_t) {
        unsafe {
            let previous = libc::signal(libc::SIGTTOU, libc::SIG_IGN);
            libc::tcsetpgrp(0, group);
            libc::signal(libc::SIGTTOU, previous);
        }
    }

    /// Returns the terminal to qk's group when dropped, if qk had it.
    pub struct Foreground {
        #[cfg(unix)]
        group: Option<libc::pid_t>,
    }

    impl Foreground {
        pub fn take_back_on_drop() -> Self {
            Self {
                #[cfg(unix)]
                group: in_foreground().then(|| unsafe { libc::getpgrp() }),
            }
        }
    }

    impl Drop for Foreground {
        fn drop(&mut self) {
            #[cfg(unix)]
            if let Some(group) = self.group {
                unsafe { foreground(group) }
            }
        }
    }
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(1))
    }
    #[cfg(not(unix))]
    {
        status.code().unwrap_or(1)
    }
}

#[cfg(all(test, unix))]
mod grace_tests {
    use super::*;

    #[test]
    fn the_kill_grace_period_is_read_as_nx_reads_it() {
        assert_eq!(grace_period(None), Duration::from_millis(5000));
        assert_eq!(grace_period(Some("250")), Duration::from_millis(250));
        assert_eq!(grace_period(Some("soon")), Duration::from_millis(5000));
    }
}
