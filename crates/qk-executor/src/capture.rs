use std::fs::File;
use std::io::{self, Read, Write};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// How a task's output reaches the terminal, after Nx's output styles.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Display {
    /// Straight through, as the task writes it: Nx's `stream-without-prefixes`.
    #[default]
    Stream,
    /// Each non-empty line behind the given prefix: Nx's `stream`.
    Prefixed(String),
    /// Held until the task ends, then printed under `> qk run <id>`: Nx's
    /// `static`. `group` folds each task into a GitHub Actions log group.
    Static { id: String, group: bool },
    /// Not shown at all.
    Hidden,
}

/// How a task ended, for the header of held output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shown {
    Success,
    Failure,
    LocalCache,
    RemoteCache,
    /// A local hit whose outputs were already in place.
    Kept,
}

impl Display {
    /// The display for a task under an output style. Continuous tasks never end
    /// on their own, so held output would never appear; they stream prefixed.
    pub fn for_task(style: OutputStyle, id: &str, project: &str, continuous: bool) -> Self {
        match style {
            OutputStyle::StreamWithoutPrefixes => Self::Stream,
            OutputStyle::Static if !continuous => Self::Static {
                id: id.to_owned(),
                group: std::env::var_os("GITHUB_ACTIONS").is_some()
                    && std::env::var_os("NX_SKIP_LOG_GROUPING").is_none_or(|value| value != "true"),
            },
            OutputStyle::Static | OutputStyle::Stream => Self::Prefixed(prefix(project)),
        }
    }
}

/// The output styles qk renders.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputStyle {
    Stream,
    StreamWithoutPrefixes,
    Static,
}

/// `project:` in bold and in the colour Nx picks for the project, when the
/// terminal takes colour.
fn prefix(project: &str) -> String {
    // picocolors' green, greenBright, blue, blueBright, cyan, cyanBright,
    // yellow, yellowBright, magenta, magentaBright, indexed like Nx's getColor.
    const COLOURS: [u8; 10] = [32, 92, 34, 94, 36, 96, 33, 93, 35, 95];
    if !colour() {
        return format!("{project}:");
    }
    let code: u32 = project.encode_utf16().map(u32::from).sum();
    let colour = COLOURS[code as usize % COLOURS.len()];
    format!("\x1b[1m\x1b[{colour}m{project}:\x1b[39m\x1b[22m")
}

/// picocolors' rule: colour unless NO_COLOR, when forced, in CI, or on a
/// terminal that is not dumb.
fn colour() -> bool {
    use std::io::IsTerminal;
    let set = |name| std::env::var_os(name).is_some();
    !set("NO_COLOR")
        && (set("FORCE_COLOR")
            || cfg!(windows)
            || set("CI")
            || (io::stdout().is_terminal()
                && std::env::var("TERM").is_ok_and(|term| term != "dumb")))
}

/// Writes one task's output as its display asks.
struct Printer {
    display: Display,
    /// Partial lines of stdout and stderr, for prefixing whole lines.
    lines: Mutex<[Vec<u8>; 2]>,
    held: Mutex<Vec<u8>>,
}

impl Printer {
    fn new(display: Display) -> Self {
        Self {
            display,
            lines: Mutex::default(),
            held: Mutex::default(),
        }
    }

    fn write(&self, stderr: bool, bytes: &[u8]) -> io::Result<()> {
        match &self.display {
            Display::Stream => write_to(stderr, bytes),
            Display::Hidden => Ok(()),
            Display::Static { .. } => {
                self.held.lock().unwrap().extend_from_slice(bytes);
                Ok(())
            }
            Display::Prefixed(prefix) => {
                let mut lines = self.lines.lock().unwrap();
                let buffer = &mut lines[usize::from(stderr)];
                buffer.extend_from_slice(bytes);
                let Some(end) = buffer
                    .iter()
                    .rposition(|byte| matches!(byte, b'\n' | b'\r'))
                else {
                    return Ok(());
                };
                let complete: Vec<u8> = buffer.drain(..=end).collect();
                write_to(stderr, &prefixed(prefix, &complete))
            }
        }
    }

    fn finish(&self, shown: Shown) -> io::Result<()> {
        match &self.display {
            Display::Stream | Display::Hidden => Ok(()),
            Display::Prefixed(prefix) => {
                let mut lines = self.lines.lock().unwrap();
                for stderr in [false, true] {
                    let rest = std::mem::take(&mut lines[usize::from(stderr)]);
                    write_to(stderr, &prefixed(prefix, &rest))?;
                }
                Ok(())
            }
            Display::Static { id, group } => {
                let held = std::mem::take(&mut *self.held.lock().unwrap());
                let status = match shown {
                    Shown::LocalCache => "  [local cache]",
                    Shown::RemoteCache => "  [remote cache]",
                    Shown::Kept => "  [existing outputs match the cache, left as is]",
                    Shown::Success | Shown::Failure => "",
                };
                let mut text = Vec::new();
                text.push(b'\n');
                if *group {
                    let icon = match shown {
                        Shown::Success => "✅",
                        Shown::Failure => "❌",
                        Shown::LocalCache | Shown::RemoteCache => "🔁",
                        Shown::Kept => "⏩",
                    };
                    text.extend_from_slice(format!("::group::{icon} ").as_bytes());
                }
                text.extend_from_slice(format!("> qk run {id}{status}\n\n").as_bytes());
                text.extend_from_slice(&held);
                if *group {
                    if !held.ends_with(b"\n") && !held.is_empty() {
                        text.push(b'\n');
                    }
                    text.extend_from_slice(b"::endgroup::\n");
                }
                write_to(false, &text)
            }
        }
    }
}

/// Nx's `formatPrefixedLines`: every non-empty line behind the prefix.
fn prefixed(prefix: &str, bytes: &[u8]) -> Vec<u8> {
    let mut result = Vec::new();
    for line in bytes.split(|byte| matches!(byte, b'\n' | b'\r')) {
        if !line.is_empty() {
            result.extend_from_slice(prefix.as_bytes());
            result.push(b' ');
            result.extend_from_slice(line);
            result.push(b'\n');
        }
    }
    result
}

fn write_to(stderr: bool, bytes: &[u8]) -> io::Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    if stderr {
        io::stderr().lock().write_all(bytes)
    } else {
        io::stdout().lock().write_all(bytes)
    }
}

/// Framed stdout/stderr chunks, optionally stored on disk without buffering the
/// task's log, and shown as the task's display asks.
pub struct Capture {
    file: Option<Mutex<File>>,
    printer: Printer,
    healthy: AtomicBool,
}

impl Capture {
    pub fn new(file: Option<File>, display: Display) -> Self {
        Self {
            file: file.map(Mutex::new),
            printer: Printer::new(display),
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
            if let Some(file) = &self.file {
                let mut file = file
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
            }
            self.printer.write(stderr, &buffer[..count])?;
        }
    }

    pub(crate) fn finish(&self, shown: Shown) -> io::Result<()> {
        self.printer.finish(shown)
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

/// Shows a recorded log as a cache hit.
pub fn replay(reader: impl Read, display: &Display, shown: Shown) -> io::Result<()> {
    let printer = Printer::new(display.clone());
    read_capture(reader, |stderr, bytes| printer.write(stderr, bytes))?;
    printer.finish(shown)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes_non_empty_lines_like_nx() {
        assert_eq!(
            prefixed("web:", b"one\n\ntwo\r\nthree"),
            b"web: one\nweb: two\nweb: three\n"
        );
    }
}
