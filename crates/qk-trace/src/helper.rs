//! The privileged helper: loads the kernel program and forwards its events
//! to the qk that started it, which does everything else unprivileged.
//!
//! It reads a line naming the prctl option and secret a process names to
//! join a recording, says it is ready once the program is attached, then
//! forwards every event as it arrives. Asked to flush, it forwards what the
//! ring buffer holds and says so, so that qk knows it has every event of a
//! command that has finished. It ends when its input does.
//!
//! Its output is frames: a kind byte, a little-endian `u32` length, and that
//! many bytes.

use std::io::{BufRead, Write};
use std::os::fd::AsRawFd;
use std::sync::mpsc;

use anyhow::{Context, Result};

use crate::program::Program;

pub(crate) const READY: u8 = 0;
pub(crate) const EVENT: u8 = 1;
/// A flush done: its id, how many events of the group it was asked for have
/// been dropped, and how many of a group the program could not count apart,
/// each a little-endian `u64`.
pub(crate) const FLUSHED: u8 = 2;
pub(crate) const FAILED: u8 = 3;

fn frame(output: &mut impl Write, kind: u8, payload: &[u8]) -> std::io::Result<()> {
    output.write_all(&[kind])?;
    output.write_all(&(payload.len() as u32).to_le_bytes())?;
    output.write_all(payload)
}

/// Runs the helper over `input` and `output`, its standard input and output.
pub fn run(input: impl BufRead + Send + 'static, mut output: impl Write) -> Result<()> {
    let mut lines = input.lines();
    let settings = lines.next().context("no settings")??;
    let (option, secret) = settings
        .split_once(' ')
        .context("settings are an option and a secret")?;
    let mut program = match Program::load(option.parse()?, secret.parse()?) {
        Ok(program) => program,
        Err(error) => {
            frame(&mut output, FAILED, format!("{error:#}").as_bytes())?;
            output.flush()?;
            return Err(error);
        }
    };
    frame(&mut output, READY, &[])?;
    output.flush()?;
    let (flushes, requested) = mpsc::channel::<(u64, u32)>();
    std::thread::spawn(move || {
        for line in lines {
            let Ok(line) = line else { break };
            // `flush <id> <group>`.
            if let Some((id, group)) = line
                .strip_prefix("flush ")
                .and_then(|rest| rest.split_once(' '))
                .and_then(|(id, group)| Some((id.parse().ok()?, group.parse().ok()?)))
                && flushes.send((id, group)).is_err()
            {
                break;
            }
        }
    });
    let ring = program.events.as_raw_fd();
    loop {
        while let Some(event) = program.events.next() {
            frame(&mut output, EVENT, &event)?;
        }
        output.flush()?;
        loop {
            match requested.try_recv() {
                Ok((id, group)) => {
                    while let Some(event) = program.events.next() {
                        frame(&mut output, EVENT, &event)?;
                    }
                    let mut payload = id.to_le_bytes().to_vec();
                    // A count it cannot read stops it, which fails every
                    // recording still waiting.
                    let (dropped, unattributed) = program.dropped(group)?;
                    // Only a group that lost events has a count to forget.
                    // One left behind costs a slot of the map, not this
                    // flush.
                    if dropped > 0 {
                        let _ = program.forget(group);
                    }
                    payload.extend_from_slice(&dropped.to_le_bytes());
                    payload.extend_from_slice(&unattributed.to_le_bytes());
                    frame(&mut output, FLUSHED, &payload)?;
                    output.flush()?;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
            }
        }
        // Wakes for events, or after a while to look for flushes.
        let mut poll = libc::pollfd {
            fd: ring,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd.
        unsafe { libc::poll(&mut poll, 1, 10) };
    }
}
