//! Pseudo-terminals for commands whose output qk captures, so tools that
//! check for a terminal behave as they do on one, as under Nx.

use std::ffi::{CStr, OsStr};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// Both ends of a pseudo-terminal. Std opens every file close-on-exec, so
/// commands started beside this one never inherit either end.
struct Pty {
    leader: File,
    follower: File,
}

fn check(result: libc::c_int) -> io::Result<()> {
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

impl Pty {
    /// A pseudo-terminal as large as qk's terminal, which passes `\n`
    /// through as it is written, as a pipe does, so that prefixes, `readyWhen`
    /// and recorded logs see the lines they see from a pipe.
    fn open() -> io::Result<Self> {
        let open = |path: &OsStr| {
            OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NOCTTY)
                .open(path)
        };
        let leader = open(OsStr::new("/dev/ptmx"))?;
        let fd = leader.as_raw_fd();
        let path = unsafe {
            check(libc::grantpt(fd))?;
            check(libc::unlockpt(fd))?;
            // ptsname answers in a static buffer; qk is its only caller.
            static NAME: Mutex<()> = Mutex::new(());
            let _name = NAME.lock().unwrap();
            let name = libc::ptsname(fd);
            if name.is_null() {
                return Err(io::Error::last_os_error());
            }
            PathBuf::from(OsStr::from_bytes(CStr::from_ptr(name).to_bytes()))
        };
        let follower = open(path.as_os_str())?;
        let fd = follower.as_raw_fd();
        unsafe {
            let mut size: libc::winsize = std::mem::zeroed();
            if libc::ioctl(1, libc::TIOCGWINSZ, &mut size) == -1 || size.ws_col == 0 {
                size = libc::winsize {
                    ws_row: 24,
                    ws_col: 80,
                    ws_xpixel: 0,
                    ws_ypixel: 0,
                };
            }
            check(libc::ioctl(fd, libc::TIOCSWINSZ, &size))?;
            let mut termios: libc::termios = std::mem::zeroed();
            check(libc::tcgetattr(fd, &mut termios))?;
            termios.c_oflag &= !libc::OPOST;
            check(libc::tcsetattr(fd, libc::TCSANOW, &termios))?;
        }
        Ok(Self { leader, follower })
    }
}

/// Starts `command` under a new pseudo-terminal, as the leader of a session
/// whose controlling terminal it is, as portable-pty starts Nx's commands. The
/// terminal is its stdout and stderr, and its stdin when `input`; otherwise
/// stdin stays as `command` sets it. Returns the terminal's leader end, from
/// which its output is read and to which its input is written.
pub fn spawn(command: &mut Command, input: bool) -> io::Result<(Session, File)> {
    let pty = Pty::open()?;
    if input {
        command.stdin(pty.follower.try_clone()?);
    }
    command
        .stdout(pty.follower.try_clone()?)
        .stderr(pty.follower.try_clone()?);
    // Between fork and exec: only system calls. A new session also starts a
    // process group of the same id, which signals and waits address.
    unsafe {
        command.pre_exec(|| {
            check(libc::setsid())?;
            check(libc::ioctl(1, libc::TIOCSCTTY as _, 0))
        });
    }
    let child = command.spawn()?;
    // Only the command may hold the follower, so reading the leader ends
    // when the command and what it started have closed it: `command` keeps
    // the copies it was given until they are replaced.
    if input {
        command.stdin(Stdio::null());
    }
    command.stdout(Stdio::null()).stderr(Stdio::null());
    drop(pty.follower);
    Ok((
        Session {
            child,
            status: None,
        },
        pty.leader,
    ))
}

/// A command leading its own session, handled as command-group handles a
/// process group: signals go to the group, and waiting reaps it.
pub struct Session {
    child: Child,
    status: Option<ExitStatus>,
}

impl Session {
    pub fn id(&self) -> u32 {
        self.child.id()
    }

    pub fn inner(&mut self) -> &mut Child {
        &mut self.child
    }

    pub fn signal(&self, signal: libc::c_int) -> io::Result<()> {
        check(unsafe { libc::killpg(self.child.id() as libc::pid_t, signal) })
    }

    pub fn kill(&mut self) -> io::Result<()> {
        self.signal(libc::SIGKILL)
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.reap(libc::WNOHANG)?;
        Ok(self.status)
    }

    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        self.reap(0)?;
        self.status
            .ok_or_else(|| io::Error::other("command exited without a status"))
    }

    /// Reaps whatever of the group has exited, keeping the leader's status;
    /// without `WNOHANG`, until the whole group has.
    fn reap(&mut self, flags: libc::c_int) -> io::Result<()> {
        let group = self.child.id() as libc::pid_t;
        loop {
            let mut status = 0;
            match unsafe { libc::waitpid(-group, &mut status, flags) } {
                0 => return Ok(()),
                -1 => {
                    let error = io::Error::last_os_error();
                    return match error.raw_os_error() {
                        Some(libc::ECHILD) => Ok(()),
                        Some(libc::EINTR) => continue,
                        _ => Err(error),
                    };
                }
                pid if pid == group => self.status = Some(ExitStatus::from_raw(status)),
                _ => {}
            }
        }
    }
}

/// The leader end's output. Linux answers `EIO` once the last follower
/// closes, which is the end of the output.
pub struct Output(pub File);

impl Read for Output {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self.0.read(buffer) {
            Err(error) if error.raw_os_error() == Some(libc::EIO) => Ok(0),
            result => result,
        }
    }
}

/// Whether the interactive command reads qk's input through its terminal:
/// when that input is a terminal too. Other input, such as a pipe, it reads
/// directly, so it sees where the input ends.
pub fn forwards_input() -> bool {
    io::IsTerminal::is_terminal(&io::stdin())
}

/// Copies qk's stdin into a pseudo-terminal while a command reads it, with
/// qk's terminal in raw input mode when `raw`, so each key reaches the
/// command as typed, Ctrl-C included, as Nx forwards input. Dropping it stops
/// the copy and restores the terminal.
pub struct Forwarder {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    saved: Option<libc::termios>,
}

impl Forwarder {
    pub fn start(mut leader: File, raw: bool) -> Self {
        let saved = raw.then(raw_input).flatten();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        // Not scoped: a read may be waiting on the terminal when the command
        // ends, so the thread polls and stops between reads.
        let thread = std::thread::spawn(move || {
            let mut buffer = [0u8; 1024];
            while !stopped.load(Ordering::SeqCst) {
                let mut poll = libc::pollfd {
                    fd: 0,
                    events: libc::POLLIN,
                    revents: 0,
                };
                match unsafe { libc::poll(&mut poll, 1, 50) } {
                    0 => continue,
                    -1 if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => {
                        continue;
                    }
                    -1 => return,
                    _ => {}
                }
                // Unbuffered, so what poll reports is what is read: std's
                // `Stdin` would keep what it read ahead out of the next poll.
                let count = unsafe { libc::read(0, buffer.as_mut_ptr().cast(), buffer.len()) };
                match count {
                    -1 if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => {}
                    // A terminal reads no end unless it hangs up.
                    0 | -1 => return,
                    count => {
                        if leader.write_all(&buffer[..count as usize]).is_err() {
                            return;
                        }
                    }
                }
            }
        });
        Self {
            stop,
            thread: Some(thread),
            saved,
        }
    }
}

impl Drop for Forwarder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        if let Some(saved) = &self.saved {
            unsafe {
                libc::tcsetattr(0, libc::TCSANOW, saved);
            }
        }
    }
}

/// Puts qk's terminal into raw input mode and returns the settings it had,
/// when stdin is a terminal whose foreground qk holds. Output processing
/// stays on, so `\n` still starts a line.
fn raw_input() -> Option<libc::termios> {
    if !crate::process::terminal::in_foreground() {
        return None;
    }
    unsafe {
        let mut saved: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(0, &mut saved) == -1 {
            return None;
        }
        let mut raw = saved;
        raw.c_iflag &= !(libc::ICRNL | libc::IXON);
        raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG | libc::IEXTEN);
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        (libc::tcsetattr(0, libc::TCSANOW, &raw) != -1).then_some(saved)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_see_a_terminal_that_passes_lines_through() {
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg(
            "[ -t 1 ] && [ -t 2 ] && echo terminal; [ -t 0 ] || echo no input; \
             stty size <&1; (exec 3</dev/tty) && echo controlling",
        );
        command.stdin(std::process::Stdio::null());
        let (mut session, leader) = spawn(&mut command, false).unwrap();
        let mut output = Vec::new();
        Output(leader).read_to_end(&mut output).unwrap();
        assert!(session.wait().unwrap().success());
        let output = String::from_utf8(output).unwrap();
        assert!(!output.contains('\r'), "{output:?}");
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines[..2], ["terminal", "no input"], "{output:?}");
        assert!(lines[2].split(' ').all(|n| n.parse::<u32>().unwrap() > 0));
        // The terminal is the command's controlling terminal.
        assert_eq!(lines[3], "controlling", "{output:?}");
    }

    #[test]
    fn input_reaches_the_command_through_the_terminal() {
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("[ -t 0 ] && echo terminal; read line; echo got $line");
        let (mut session, mut leader) = spawn(&mut command, true).unwrap();
        leader.write_all(b"typed\n").unwrap();
        let mut output = Vec::new();
        Output(leader).read_to_end(&mut output).unwrap();
        assert!(session.wait().unwrap().success());
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("terminal\n"), "{output:?}");
        assert!(output.contains("got typed\n"), "{output:?}");
    }

    #[test]
    fn signals_reach_the_whole_session() {
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg("sleep 30 & sleep 30");
        let (mut session, leader) = spawn(&mut command, false).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));
        session.kill().unwrap();
        assert!(session.wait().unwrap().signal() == Some(libc::SIGKILL));
        // Both sleeps are gone, so the output ends.
        let mut output = Vec::new();
        Output(leader).read_to_end(&mut output).unwrap();
        assert!(session.try_wait().unwrap().is_some());
    }
}
