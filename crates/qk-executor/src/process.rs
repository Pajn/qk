use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use command_group::{CommandGroup, GroupChild};

use crate::{Capture, Display, PreparedTask, Shown};

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
    if task.display == Display::Stream {
        // Straight through: the task writes to the terminal itself.
        return execute_captured(task, cancelled, None);
    }
    let capture = Capture::new(None, task.display.clone());
    execute_captured(task, cancelled, Some(&capture))
}

pub fn execute_captured(
    task: &PreparedTask,
    cancelled: &AtomicBool,
    capture: Option<&Capture>,
) -> Result<Outcome> {
    std::thread::scope(|scope| {
        let mut readers = Vec::new();
        let outcome = (|| {
            let mut pending = task.commands.iter();
            let mut children = Children(Vec::new());
            loop {
                if cancelled.load(Ordering::SeqCst) {
                    children.terminate();
                    return Ok(Outcome::Cancelled);
                }
                if children.0.is_empty() || task.parallel {
                    for command in pending.by_ref() {
                        let mut child = spawn(task, command, capture.is_some())
                            .with_context(|| format!("cannot start command for {}", task.id))?;
                        if let Some(capture) = capture {
                            let stdout =
                                child.inner().stdout.take().context("missing stdout pipe")?;
                            let stderr =
                                child.inner().stderr.take().context("missing stderr pipe")?;
                            readers.push(scope.spawn(move || capture.copy(stdout, false)));
                            readers.push(scope.spawn(move || capture.copy(stderr, true)));
                        }
                        children.0.push(child);
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
                                return Ok(Outcome::Failed(exit_code(status)));
                            }
                        }
                        None => index += 1,
                    }
                }
                std::thread::sleep(Duration::from_millis(20));
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

fn spawn(task: &PreparedTask, text: &str, capture: bool) -> Result<GroupChild> {
    #[cfg(windows)]
    let mut command = {
        use std::os::windows::process::CommandExt;
        let mut command =
            Command::new(std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into()));
        command
            .args(["/D", "/S", "/C"])
            .raw_arg(format!("\"{text}\""));
        command
    };
    #[cfg(not(windows))]
    let mut command = {
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg(text);
        command
    };
    command
        .current_dir(&task.cwd)
        .env_clear()
        .envs(&task.env)
        .stdin(Stdio::null())
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
    Ok(command.group_spawn()?)
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
