use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use command_group::{CommandGroup, GroupChild};

use crate::PreparedTask;

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

pub fn execute(task: &PreparedTask, cancelled: &AtomicBool) -> Result<Outcome> {
    let mut pending = task.commands.iter();
    let mut children = Children(Vec::new());
    loop {
        if cancelled.load(Ordering::SeqCst) {
            return Ok(Outcome::Cancelled);
        }
        if children.0.is_empty() || task.parallel {
            for command in pending.by_ref() {
                children.0.push(
                    spawn(task, command)
                        .with_context(|| format!("cannot start command for {}", task.id))?,
                );
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
}

fn spawn(task: &PreparedTask, text: &str) -> Result<GroupChild> {
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
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
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
