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
    /// Held until the task ends, and printed under a header only if it
    /// failed: for the quiet and dynamic styles.
    Failures { id: String },
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
            OutputStyle::Quiet => Self::Failures { id: id.to_owned() },
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
    /// Only failed tasks' output, for the quiet and dynamic styles.
    Quiet,
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

/// Nx's per-command `prefix`, `prefixColor`, `color` and `bgColor`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Decoration {
    pub prefix: Option<String>,
    pub prefix_color: Option<String>,
    pub color: Option<String>,
    pub bg_color: Option<String>,
}

const COLOURS: [&str; 8] = [
    "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
];

impl Decoration {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    /// Whether `name` is a colour a command entry may name: `bg` names
    /// background colours.
    pub fn valid(name: &str, background: bool) -> bool {
        match name.strip_prefix("bg") {
            Some(rest) if background => {
                let mut chars = rest.chars();
                chars.next().is_some_and(char::is_uppercase)
                    && COLOURS.contains(&rest.to_lowercase().as_str())
            }
            _ => !background && COLOURS.contains(&name),
        }
    }

    /// As Nx's `addColorAndPrefix`: a bold, optionally coloured prefix before
    /// each non-blank line, then the whole text in `color` and `bgColor`.
    fn apply(&self, bytes: &[u8]) -> Vec<u8> {
        let colours = colour();
        let paint = |text: &str, name: Option<&str>, background: bool| -> String {
            let index = name.and_then(|name| {
                let name = if background {
                    name.get(2..)?.to_lowercase()
                } else {
                    name.to_owned()
                };
                COLOURS.iter().position(|colour| *colour == name)
            });
            match index {
                Some(index) if colours => {
                    let (open, close) = if background { (40, 49) } else { (30, 39) };
                    format!("\x1b[{}m{text}\x1b[{close}m", open + index)
                }
                _ => text.to_owned(),
            }
        };
        let mut text = String::from_utf8_lossy(bytes).into_owned();
        if let Some(prefix) = &self.prefix {
            let mut prefix = paint(prefix, self.prefix_color.as_deref(), false);
            if colours {
                prefix = format!("\x1b[1m{prefix}\x1b[22m");
            }
            text = text
                .split('\n')
                .map(|line| {
                    if line.trim().is_empty() {
                        line.to_owned()
                    } else {
                        format!("{prefix} {line}")
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
        }
        let text = paint(&text, self.color.as_deref(), false);
        paint(&text, self.bg_color.as_deref(), true).into_bytes()
    }
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
            Display::Static { .. } | Display::Failures { .. } => {
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
            Display::Failures { id } => {
                let held = std::mem::take(&mut *self.held.lock().unwrap());
                if shown == Shown::Failure {
                    let mut text = format!("\n✖ qk run {id} failed\n\n").into_bytes();
                    text.extend_from_slice(&held);
                    if !held.ends_with(b"\n") {
                        text.push(b'\n');
                    }
                    write_to(false, &text)?;
                }
                Ok(())
            }
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
    crate::report::output(stderr, bytes);
    Ok(())
}

/// Framed stdout/stderr chunks, optionally stored on disk without buffering the
/// task's log, and shown as the task's display asks.
pub struct Capture {
    file: Option<Mutex<File>>,
    retained: Mutex<Option<RecordedLog>>,
    printer: Printer,
    healthy: AtomicBool,
    readiness: Option<Readiness>,
}

/// Maximum framed bytes retained for an execution, independent of result caching.
pub const MAX_LOG_BYTES: usize = 4 * 1024 * 1024;

/// Ordered, stream-tagged frames retained for run history.
#[derive(Default)]
pub struct RecordedLog {
    pub data: Vec<u8>,
    /// Further output was displayed but omitted from this bounded record.
    pub truncated: bool,
}

impl RecordedLog {
    /// Keeps a prefix of complete frames, stopping at the payload limit.
    fn append(&mut self, stderr: bool, bytes: &[u8]) {
        if self.truncated {
            return;
        }
        let remaining = MAX_LOG_BYTES.saturating_sub(self.data.len());
        let count = bytes.len().min(remaining.saturating_sub(5));
        if count > 0 {
            self.data.push(u8::from(stderr));
            self.data.extend_from_slice(&(count as u32).to_le_bytes());
            self.data.extend_from_slice(&bytes[..count]);
        }
        self.truncated = count != bytes.len();
    }
}

/// Watches output for `readyWhen` text, on either stream and across reads.
struct Readiness {
    patterns: Vec<String>,
    /// Which patterns have appeared, and each stream's last bytes, so text
    /// split between two reads still matches.
    state: Mutex<(Vec<bool>, [Vec<u8>; 2])>,
    ready: std::sync::Arc<AtomicBool>,
}

impl Readiness {
    fn see(&self, stderr: bool, bytes: &[u8]) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let (found, tails) = &mut *state;
        let tail = &mut tails[usize::from(stderr)];
        tail.extend_from_slice(bytes);
        let text = String::from_utf8_lossy(tail);
        for (pattern, found) in self.patterns.iter().zip(found.iter_mut()) {
            *found |= text.contains(pattern.as_str());
        }
        let keep = self.patterns.iter().map(String::len).max().unwrap_or(0);
        if tail.len() > keep {
            tail.drain(..tail.len() - keep);
        }
        if found.iter().all(|found| *found) {
            self.ready.store(true, Ordering::SeqCst);
        }
    }
}

impl Capture {
    pub fn new(file: Option<File>, display: Display) -> Self {
        Self {
            file: file.map(Mutex::new),
            retained: Mutex::new(None),
            printer: Printer::new(display),
            healthy: AtomicBool::new(true),
            readiness: None,
        }
    }

    /// Retains a bounded execution log alongside any result-cache capture.
    pub fn retain_log(self) -> Self {
        *self.retained.lock().unwrap() = Some(RecordedLog::default());
        self
    }

    /// Takes the retained log after the command's output readers have finished.
    pub fn take_log(&self) -> Option<RecordedLog> {
        self.retained.lock().unwrap().take()
    }

    /// Sets `ready` once every one of `patterns` has appeared in the output.
    pub fn ready_when(mut self, patterns: &[String], ready: std::sync::Arc<AtomicBool>) -> Self {
        if !patterns.is_empty() {
            self.readiness = Some(Readiness {
                patterns: patterns.to_vec(),
                state: Mutex::new((vec![false; patterns.len()], [Vec::new(), Vec::new()])),
                ready,
            });
        }
        self
    }

    pub fn healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }

    /// Copies one command's stream, decorated as its command entry asks.
    pub(crate) fn copy(
        &self,
        mut reader: impl Read,
        stderr: bool,
        decoration: Option<&Decoration>,
    ) -> io::Result<()> {
        let decoration = decoration.filter(|decoration| !decoration.is_empty());
        let mut buffer = [0; 16 * 1024];
        // With a prefix, whole lines, so a line split between reads gets one.
        let mut partial = Vec::new();
        loop {
            let count = reader.read(&mut buffer)?;
            let chunk = &buffer[..count];
            match decoration {
                None if count == 0 => return Ok(()),
                None => self.emit(stderr, chunk)?,
                Some(decoration) if decoration.prefix.is_some() => {
                    partial.extend_from_slice(chunk);
                    let end = if count == 0 {
                        partial.len()
                    } else {
                        partial
                            .iter()
                            .rposition(|byte| *byte == b'\n')
                            .map_or(0, |end| end + 1)
                    };
                    if end > 0 {
                        let lines: Vec<u8> = partial.drain(..end).collect();
                        self.emit(stderr, &decoration.apply(&lines))?;
                    }
                    if count == 0 {
                        return Ok(());
                    }
                }
                Some(_) if count == 0 => return Ok(()),
                Some(decoration) => self.emit(stderr, &decoration.apply(chunk))?,
            }
        }
    }

    /// Records, shows and watches output, in frames the log reader accepts.
    fn emit(&self, stderr: bool, bytes: &[u8]) -> io::Result<()> {
        for frame in bytes.chunks(16 * 1024) {
            // One order for retained frames, cache frames and displayed chunks.
            let mut retained = self
                .retained
                .lock()
                .map_err(|_| io::Error::other("retained log lock poisoned"))?;
            if let Some(log) = retained.as_mut() {
                log.append(stderr, frame);
            }
            if let Some(file) = &self.file {
                let mut file = file
                    .lock()
                    .map_err(|_| io::Error::other("output capture lock poisoned"))?;
                if self.healthy() {
                    let result = (|| {
                        file.write_all(&[u8::from(stderr)])?;
                        file.write_all(&(frame.len() as u32).to_le_bytes())?;
                        file.write_all(frame)
                    })();
                    if result.is_err() {
                        self.healthy.store(false, Ordering::Relaxed);
                    }
                }
            }
            self.printer.write(stderr, frame)?;
            if let Some(readiness) = &self.readiness {
                readiness.see(stderr, frame);
            }
        }
        Ok(())
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

    /// Retention preserves binary streams and valid framing at the byte limit.
    #[test]
    fn retained_logs_are_ordered_and_bounded() {
        let capture = Capture::new(None, Display::Hidden).retain_log();
        capture.emit(false, b"stdout\0\xff").unwrap();
        capture.emit(true, b"stderr").unwrap();
        let data = vec![b'x'; MAX_LOG_BYTES];
        capture.emit(false, &data).unwrap();
        capture.emit(true, b"omitted").unwrap();
        let log = capture.take_log().unwrap();
        assert!(log.truncated);
        assert!(log.data.len() <= MAX_LOG_BYTES);
        let mut frames = Vec::new();
        read_capture(log.data.as_slice(), |stderr, bytes| {
            frames.push((stderr, bytes.to_vec()));
            Ok(())
        })
        .unwrap();
        assert_eq!(frames[0], (false, b"stdout\0\xff".to_vec()));
        assert_eq!(frames[1], (true, b"stderr".to_vec()));
        assert!(
            frames[2..]
                .iter()
                .all(|(stderr, bytes)| !stderr && bytes.iter().all(|b| *b == b'x'))
        );
        assert!(capture.take_log().is_none());
    }

    #[test]
    fn prefixes_non_empty_lines_like_nx() {
        assert_eq!(
            prefixed("web:", b"one\n\ntwo\r\nthree"),
            b"web: one\nweb: two\nweb: three\n"
        );
    }
}
