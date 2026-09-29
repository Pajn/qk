use std::fs::File;
use std::io::{self, Read, Write};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// Framed stdout/stderr chunks, stored on disk without buffering the task's log.
pub struct Capture {
    file: Mutex<File>,
    stream: bool,
    healthy: AtomicBool,
}

impl Capture {
    pub fn new(file: File, stream: bool) -> Self {
        Self {
            file: Mutex::new(file),
            stream,
            healthy: AtomicBool::new(true),
        }
    }

    pub fn healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }

    pub(crate) fn copy(&self, mut reader: impl Read, stderr: bool) -> io::Result<()> {
        let mut buffer = [0; 16 * 1024];
        loop {
            let count = reader.read(&mut buffer)?;
            if count == 0 {
                return Ok(());
            }
            let mut file = self
                .file
                .lock()
                .map_err(|_| io::Error::other("output capture lock poisoned"))?;
            if self.healthy() {
                let result = (|| {
                    file.write_all(&[u8::from(stderr)])?;
                    file.write_all(&(count as u32).to_le_bytes())?;
                    file.write_all(&buffer[..count])
                })();
                if result.is_err() {
                    self.healthy.store(false, Ordering::Relaxed);
                }
            }
            if self.stream {
                if stderr {
                    io::stderr().lock().write_all(&buffer[..count])?;
                } else {
                    io::stdout().lock().write_all(&buffer[..count])?;
                }
            }
        }
    }
}

pub fn read_capture(
    mut reader: impl Read,
    mut consume: impl FnMut(bool, &[u8]) -> io::Result<()>,
) -> io::Result<()> {
    loop {
        let mut stream = [0];
        if reader.read(&mut stream)? == 0 {
            return Ok(());
        }
        if stream[0] > 1 {
            return Err(io::Error::other("invalid output capture stream"));
        }
        let mut length = [0; 4];
        reader.read_exact(&mut length)?;
        let length = u32::from_le_bytes(length) as usize;
        if length > 16 * 1024 {
            return Err(io::Error::other("invalid output capture frame"));
        }
        let mut bytes = vec![0; length];
        reader.read_exact(&mut bytes)?;
        consume(stream[0] == 1, &bytes)?;
    }
}

pub fn replay(reader: impl Read) -> io::Result<()> {
    read_capture(reader, |stderr, bytes| {
        if stderr {
            io::stderr().lock().write_all(bytes)
        } else {
            io::stdout().lock().write_all(bytes)
        }
    })
}
