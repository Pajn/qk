use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
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
    /// `static`. `group` folds each task into a GitHub Actions log group, and
    /// `cached` shows a cache hit's log rather than only its header.
    Static {
        id: String,
        group: bool,
        cached: bool,
    },
    /// Straight through under `> qk run <id>`: the task a static `run` is for.
    Headed { id: String },
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
    /// `root` is a task the run was asked for, rather than a dependency.
    pub fn for_task(
        style: OutputStyle,
        id: &str,
        project: &str,
        continuous: bool,
        root: bool,
    ) -> Self {
        match style {
            OutputStyle::StreamWithoutPrefixes => Self::Stream,
            OutputStyle::RunOne if root && !continuous => Self::Headed { id: id.to_owned() },
            OutputStyle::Static | OutputStyle::RunOne if !continuous => Self::Static {
                id: id.to_owned(),
                group: std::env::var_os("GITHUB_ACTIONS").is_some()
                    && std::env::var_os("NX_SKIP_LOG_GROUPING").is_none_or(|value| value != "true"),
                cached: style == OutputStyle::Static,
            },
            OutputStyle::Quiet => Self::Failures { id: id.to_owned() },
            OutputStyle::Static | OutputStyle::RunOne | OutputStyle::Stream => {
                Self::Prefixed(prefix(project))
            }
        }
    }
}

/// The output styles qk renders.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputStyle {
    Stream,
    StreamWithoutPrefixes,
    Static,
    /// `static` for one requested task, as Nx shows `nx run`: the task streams
    /// under its header, and its dependencies are held, with only the headers
    /// of cache hits.
    RunOne,
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

/// Small logs stay in memory; larger logs and unfinished lines spill to an
/// anonymous file that is removed when dropped. Replay never copies the log.
const MEMORY_OUTPUT_BYTES: usize = 64 * 1024;

#[derive(Default)]
struct Spool {
    memory: Vec<u8>,
    file: Option<File>,
    last: Option<u8>,
}

impl Spool {
    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.file.is_none() && self.memory.len() + bytes.len() > MEMORY_OUTPUT_BYTES {
            let mut file = tempfile::tempfile()?;
            file.write_all(&self.memory)?;
            self.file = Some(file);
            self.memory = Vec::new();
        }
        if let Some(file) = &mut self.file {
            file.write_all(bytes)?;
        } else {
            self.memory.extend_from_slice(bytes);
        }
        if let Some(last) = bytes.last() {
            self.last = Some(*last);
        }
        Ok(())
    }

    fn visit(&mut self, mut consume: impl FnMut(&[u8]) -> io::Result<()>) -> io::Result<()> {
        if let Some(file) = &mut self.file {
            file.seek(SeekFrom::Start(0))?;
            let mut buffer = [0; 16 * 1024];
            loop {
                let count = file.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                consume(&buffer[..count])?;
            }
            // A visit need not end this spool's lifetime.
            file.seek(SeekFrom::End(0))?;
        } else {
            for bytes in self.memory.chunks(16 * 1024) {
                consume(bytes)?;
            }
        }
        Ok(())
    }
    fn prefixed(&mut self, stderr: bool, prefix: &str) -> io::Result<()> {
        if self.file.is_none() {
            return write_to(stderr, &prefixed(prefix, &self.memory));
        }
        crate::report::output_batch(|output| {
            let mut started = false;
            self.visit(|bytes| {
                for piece in bytes.split_inclusive(|byte| matches!(byte, b'\n' | b'\r')) {
                    let complete = piece
                        .last()
                        .is_some_and(|byte| matches!(byte, b'\n' | b'\r'));
                    let content = if complete {
                        &piece[..piece.len() - 1]
                    } else {
                        piece
                    };
                    if !content.is_empty() {
                        if !started {
                            output(stderr, prefix.as_bytes());
                            output(stderr, b" ");
                            started = true;
                        }
                        output(stderr, content);
                    }
                    if complete && started {
                        output(stderr, b"\n");
                        started = false;
                    }
                }
                Ok(())
            })?;
            if started {
                output(stderr, b"\n");
            }
            Ok(())
        })
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
    lines: Mutex<[Spool; 2]>,
    held: Mutex<Spool>,
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
            Display::Stream | Display::Headed { .. } => write_to(stderr, bytes),
            Display::Hidden => Ok(()),
            Display::Static { .. } | Display::Failures { .. } => {
                self.held.lock().unwrap().append(bytes)
            }
            Display::Prefixed(prefix) => {
                let mut lines = self.lines.lock().unwrap();
                let buffer = &mut lines[usize::from(stderr)];
                let end = bytes
                    .iter()
                    .rposition(|byte| matches!(byte, b'\n' | b'\r'))
                    .map(|end| end + 1);
                if let Some(end) = end {
                    buffer.append(&bytes[..end])?;
                    let mut complete = std::mem::take(buffer);
                    complete.prefixed(stderr, prefix)?;
                    buffer.append(&bytes[end..])?;
                } else {
                    buffer.append(bytes)?;
                }
                Ok(())
            }
        }
    }

    /// Announces a task about to show its output; `shown` for a cache hit.
    fn begin(&self, shown: Option<Shown>) -> io::Result<()> {
        match &self.display {
            Display::Headed { id } => {
                let header = header(id, shown.unwrap_or(Shown::Success));
                write_to(false, format!("\n{header}\n\n").as_bytes())
            }
            _ => Ok(()),
        }
    }

    fn finish(&self, shown: Shown) -> io::Result<()> {
        match &self.display {
            Display::Stream | Display::Headed { .. } | Display::Hidden => Ok(()),
            Display::Failures { id } => {
                let mut held = std::mem::take(&mut *self.held.lock().unwrap());
                if shown == Shown::Failure {
                    crate::report::output_batch(|output| -> io::Result<()> {
                        output(false, format!("\n✖ qk run {id} failed\n\n").as_bytes());
                        held.visit(|bytes| {
                            output(false, bytes);
                            Ok(())
                        })?;
                        if held.last != Some(b'\n') {
                            output(false, b"\n");
                        }
                        Ok(())
                    })?;
                }
                Ok(())
            }
            Display::Prefixed(prefix) => {
                let mut lines = self.lines.lock().unwrap();
                for stderr in [false, true] {
                    let mut rest = std::mem::take(&mut lines[usize::from(stderr)]);
                    rest.prefixed(stderr, prefix)?;
                }
                Ok(())
            }
            Display::Static { id, group, cached } => {
                let mut held = std::mem::take(&mut *self.held.lock().unwrap());
                if !cached && matches!(shown, Shown::LocalCache | Shown::RemoteCache | Shown::Kept)
                {
                    held = Spool::default();
                }
                crate::report::output_batch(|output| -> io::Result<()> {
                    output(false, b"\n");
                    if *group {
                        let icon = match shown {
                            Shown::Success => "✅",
                            Shown::Failure => "❌",
                            Shown::LocalCache | Shown::RemoteCache => "🔁",
                            Shown::Kept => "⏩",
                        };
                        output(false, format!("::group::{icon} ").as_bytes());
                    }
                    output(false, format!("{}\n\n", header(id, shown)).as_bytes());
                    held.visit(|bytes| {
                        output(false, bytes);
                        Ok(())
                    })?;
                    if *group {
                        if held.last.is_some() && held.last != Some(b'\n') {
                            output(false, b"\n");
                        }
                        output(false, b"::endgroup::\n");
                    }
                    Ok(())
                })
            }
        }
    }
}

/// `> qk run <id>`, marked when the output came from the cache.
fn header(id: &str, shown: Shown) -> String {
    let status = match shown {
        Shown::LocalCache => "  [local cache]",
        Shown::RemoteCache => "  [remote cache]",
        Shown::Kept => "  [existing outputs match the cache, left as is]",
        Shown::Success | Shown::Failure => "",
    };
    format!("> qk run {id}{status}")
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
    /// Streams output through its display, optionally recording cache frames on disk.
    /// A headed display announces the task here, as it starts.
    pub fn new(file: Option<File>, display: Display) -> Self {
        let printer = Printer::new(display);
        let _ = printer.begin(None);
        Self {
            file: file.map(Mutex::new),
            retained: Mutex::new(None),
            printer,
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
    printer.begin(Some(shown))?;
    read_capture(reader, |stderr, bytes| printer.write(stderr, bytes))?;
    printer.finish(shown)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn held_output_spills_without_losing_binary_stream_order() {
        let printer = Printer::new(Display::Failures { id: "large".into() });
        let chunk = [0, 255, b'x', b'\n'].repeat(4096);
        for index in 0..32 {
            printer.write(index % 2 == 1, &chunk).unwrap();
        }
        let mut held = printer.held.lock().unwrap();
        assert!(held.file.is_some());
        assert!(held.memory.is_empty());
        let mut count = 0;
        held.visit(|bytes| {
            assert_eq!(bytes, chunk.as_slice());
            count += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(count, 32);
        // A successful quiet task drops its spool without reading or displaying it.
        drop(held);
        printer.finish(Shown::Success).unwrap();
        assert!(printer.held.lock().unwrap().file.is_none());
    }

    #[test]
    fn unfinished_prefixed_lines_spill_and_keep_streams_separate() {
        let printer = Printer::new(Display::Prefixed("app:".into()));
        let bytes = vec![b'x'; MEMORY_OUTPUT_BYTES + 1];
        printer.write(false, &bytes).unwrap();
        printer.write(true, b"partial stderr").unwrap();
        let mut lines = printer.lines.lock().unwrap();
        assert!(lines[0].file.is_some());
        assert!(lines[0].memory.is_empty());
        let mut length = 0;
        lines[0]
            .visit(|chunk| {
                assert!(chunk.len() <= 16 * 1024);
                assert!(chunk.iter().all(|byte| *byte == b'x'));
                length += chunk.len();
                Ok(())
            })
            .unwrap();
        assert_eq!(length, bytes.len());
        assert_eq!(lines[1].memory, b"partial stderr");
    }

    #[test]
    fn small_spools_stay_in_memory_and_can_replay_then_append() {
        let mut spool = Spool::default();
        spool.append(b"small").unwrap();
        assert!(spool.file.is_none());
        let mut output = Vec::new();
        spool
            .visit(|chunk| {
                output.extend_from_slice(chunk);
                Ok(())
            })
            .unwrap();
        assert_eq!(output, b"small");
        spool.append(&vec![b'x'; MEMORY_OUTPUT_BYTES]).unwrap();
        assert!(spool.file.is_some());
        spool.visit(|_| Ok(())).unwrap();
        spool.append(b"last").unwrap();
        output.clear();
        spool
            .visit(|chunk| {
                output.extend_from_slice(chunk);
                Ok(())
            })
            .unwrap();
        assert!(output.starts_with(b"small"));
        assert!(output.ends_with(b"last"));
    }

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
