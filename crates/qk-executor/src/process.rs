use std::process::{Command, Stdio};
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

struct Children(Vec<GroupChild>);

impl Drop for Children {
    fn drop(&mut self) {
        // Also clean up when spawning or polling one command fails.
        for child in &mut self.0 {
            let _ = child.kill();
        }
        for child in &mut self.0 {
            let _ = child.wait();
        }
    }
}

/// How long a cancelled or stopped command may take to exit after SIGTERM.
#[cfg(unix)]
const GRACE: Duration = Duration::from_secs(5);

impl Children {
    /// Asks every process group to exit, then leaves stragglers to `Drop`.
    fn terminate(&mut self) {
        #[cfg(unix)]
        {
            use command_group::{Signal, UnixChildExt};
            for child in &self.0 {
                let _ = child.signal(Signal::SIGTERM);
            }
            let deadline = std::time::Instant::now() + GRACE;
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
    if task.display == Display::Stream
        && task.ready_when.is_empty()
        && task.decorations.iter().all(Decoration::is_empty)
    {
        // Straight through: the task writes to the terminal itself.
        return execute_captured(task, cancelled, None);
    }
    let capture =
        Capture::new(None, task.display.clone()).ready_when(&task.ready_when, task.ready.clone());
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
            let mut pending = task.commands.iter().enumerate();
            let mut children = Children(Vec::new());
            let mut launched = Instant::now();
            loop {
                if cancelled.load(Ordering::SeqCst) {
                    children.terminate();
                    return Ok(Outcome::Cancelled);
                }
                if children.0.is_empty() || task.parallel {
                    for (index, command) in pending.by_ref() {
                        let interactive = task.interactive && task.commands.len() == 1;
                        let mut child = spawn(task, command, index, capture.is_some(), interactive)
                            .with_context(|| format!("cannot start command for {}", task.id))?;
                        if let Some(capture) = capture {
                            let stdout =
                                child.inner().stdout.take().context("missing stdout pipe")?;
                            let stderr =
                                child.inner().stderr.take().context("missing stderr pipe")?;
                            let decoration = task.decorations.get(index);
                            readers
                                .push(scope.spawn(move || capture.copy(stdout, false, decoration)));
                            readers
                                .push(scope.spawn(move || capture.copy(stderr, true, decoration)));
                        }
                        children.0.push(child);
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
                            let mut child = children.0.swap_remove(index);
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
fn spawn(
    task: &PreparedTask,
    text: &str,
    index: usize,
    capture: bool,
    interactive: bool,
) -> Result<GroupChild> {
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
        command = recording.command(index, text);
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
    Ok(command.group_spawn()?)
}

/// Handing the terminal to an interactive command and back.
mod terminal {
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
