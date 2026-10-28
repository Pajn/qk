//! How a run looks: the live panel on a terminal, the quiet style for agents
//! and scripts, and the summary every multi-task run ends with.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use qk_executor::Outcome;
use qk_executor::report::{self, Cached, Event, Sink};
use qk_history::RunReport;
use qk_runner::RunResult;

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Colours for stderr, following the same rule as task output.
#[derive(Clone, Copy)]
pub struct Paint(bool);

impl Paint {
    /// Colours on a terminal, or when FORCE_COLOR asks for them; never under
    /// NO_COLOR or `FORCE_COLOR=0`.
    pub fn stderr() -> Self {
        let set = |name| std::env::var_os(name).is_some();
        let forced = qk_executor::force_colour();
        Self(
            !set("NO_COLOR")
                && forced != Some(false)
                && (forced == Some(true) || std::io::stderr().is_terminal()),
        )
    }

    /// Colours for stdout on a terminal, as `stderr` decides for stderr.
    pub fn stdout() -> Self {
        let set = |name| std::env::var_os(name).is_some();
        let forced = qk_executor::force_colour();
        Self(
            !set("NO_COLOR")
                && forced != Some(false)
                && (forced == Some(true) || std::io::stdout().is_terminal()),
        )
    }

    /// Colours for a static log, following picocolors as task output does:
    /// also in CI, whose log viewers render them.
    pub fn log() -> Self {
        let set = |name| std::env::var_os(name).is_some();
        let forced = qk_executor::force_colour();
        Self(
            !set("NO_COLOR")
                && forced != Some(false)
                && (forced == Some(true)
                    || set("CI")
                    || (std::io::stderr().is_terminal()
                        && std::env::var("TERM").is_ok_and(|term| term != "dumb"))),
        )
    }

    fn wrap(self, code: &str, text: &str) -> String {
        if self.0 {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    }

    pub fn bold(self, text: &str) -> String {
        self.wrap("1", text)
    }
    pub fn dim(self, text: &str) -> String {
        self.wrap("2", text)
    }
    fn red(self, text: &str) -> String {
        self.wrap("31", text)
    }
    fn green(self, text: &str) -> String {
        self.wrap("32", text)
    }
    fn yellow(self, text: &str) -> String {
        self.wrap("33", text)
    }
    fn cyan(self, text: &str) -> String {
        self.wrap("36", text)
    }
}

/// Warnings kept for the summary instead of interrupting the run.
#[derive(Default)]
struct Warnings(Mutex<Vec<String>>);

impl Warnings {
    fn push(&self, line: &str) {
        let mut warnings = self.0.lock().unwrap();
        if !warnings.iter().any(|known| known == line) {
            warnings.push(line.to_owned());
        }
    }
}

/// Nothing while tasks run; failed tasks' output as they fail, since their
/// display holds it, and warnings in the summary. Tasks colour their output
/// even when piped, as Nx asks them to, so escape sequences are removed when
/// the output is not a terminal and no one asked for colour.
pub struct Quiet {
    warnings: Warnings,
    plain: [bool; 2],
    escapes: Mutex<[EscapeState; 2]>,
}

impl Default for Quiet {
    fn default() -> Self {
        let plain = |terminal: bool| match qk_executor::force_colour() {
            Some(forced) => !forced,
            None => !terminal,
        };
        Self {
            warnings: Warnings::default(),
            escapes: Mutex::default(),
            plain: [
                plain(std::io::stdout().is_terminal()),
                plain(std::io::stderr().is_terminal()),
            ],
        }
    }
}

/// Incremental ANSI removal: held logs replay in bounded chunks, so the
/// escape introducer and its final character can arrive in different writes.
#[derive(Clone, Copy, Default)]
enum EscapeState {
    #[default]
    Text,
    Escape,
    Csi,
    String {
        osc: bool,
    },
    StringEscape {
        osc: bool,
    },
}

impl EscapeState {
    fn strip(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut result = Vec::with_capacity(bytes.len());
        for &byte in bytes {
            match *self {
                Self::Csi => {
                    if (0x40..=0x7e).contains(&byte) {
                        *self = Self::Text;
                    }
                }
                Self::Escape if byte == b'[' => *self = Self::Csi,
                Self::Escape if matches!(byte, b']' | b'P' | b'X' | b'^' | b'_') => {
                    *self = Self::String { osc: byte == b']' };
                }
                Self::String { osc } | Self::StringEscape { osc }
                    if (osc && byte == 0x07)
                        || (matches!(*self, Self::StringEscape { .. }) && byte == b'\\') =>
                {
                    *self = Self::Text;
                }
                Self::String { osc } | Self::StringEscape { osc } => {
                    *self = if byte == 0x1b {
                        Self::StringEscape { osc }
                    } else {
                        Self::String { osc }
                    };
                }
                Self::Escape => {
                    result.push(0x1b);
                    *self = Self::Text;
                    if byte == 0x1b {
                        *self = Self::Escape;
                    } else {
                        result.push(byte);
                    }
                }
                Self::Text if byte == 0x1b => *self = Self::Escape,
                Self::Text => result.push(byte),
            }
        }
        result
    }

    /// Preserve a literal trailing escape; discard unfinished control sequences.
    /// Never carry them into another task.
    fn finish(&mut self) -> bool {
        let trailing = matches!(self, Self::Escape);
        *self = Self::Text;
        trailing
    }
}

impl Sink for Quiet {
    fn event(&self, _: &Event) {}

    fn warning(&self, line: &str) {
        self.warnings.push(line);
    }

    fn output(&self, stderr: bool, bytes: &[u8]) {
        if self.plain[usize::from(stderr)] {
            let mut escapes = self.escapes.lock().unwrap();
            report::write_all(stderr, &escapes[usize::from(stderr)].strip(bytes));
        } else {
            report::write_all(stderr, bytes);
        }
    }
    fn output_finished(&self) {
        let mut escapes = self.escapes.lock().unwrap();
        for stderr in [false, true] {
            if self.plain[usize::from(stderr)] && escapes[usize::from(stderr)].finish() {
                report::write_all(stderr, b"\x1b");
            }
        }
    }
}

/// The static styles: task headers say how each task went, so events print
/// nothing; warnings print as they come, and output passes through.
pub struct Static;

impl Sink for Static {
    fn event(&self, _: &Event) {}

    fn warning(&self, line: &str) {
        report::write_all(true, format!("{line}\n").as_bytes());
    }

    fn output(&self, stderr: bool, bytes: &[u8]) {
        report::write_all(stderr, bytes);
    }
}

/// What a run was asked for, as Nx's static styles word it: `target build
/// for project web and 2 tasks it depends on`.
pub struct Title {
    single: bool,
    projects: Vec<String>,
    text: String,
}

impl Title {
    pub fn new(graph: &qk_taskgraph::TaskGraph, single: bool) -> Self {
        let mut projects = Vec::new();
        let mut targets = Vec::new();
        for id in &graph.roots {
            if let Some(task) = graph.tasks.get(id) {
                if !projects.contains(&task.project) {
                    projects.push(task.project.clone());
                }
                if !targets.contains(&task.target) {
                    targets.push(task.target.clone());
                }
            }
        }
        let mut text = if let [target] = &targets[..] {
            format!("target {target}")
        } else {
            format!("targets {}", targets.join(", "))
        };
        if let [project] = &projects[..] {
            text.push_str(&format!(" for project {project}"));
        } else {
            text.push_str(&format!(" for {} projects", projects.len()));
        }
        let dependencies = graph.tasks.len().saturating_sub(graph.roots.len());
        if dependencies > 0 {
            text.push_str(&format!(
                " and {dependencies} task{} {} depend{} on",
                if dependencies == 1 { "" } else { "s" },
                if projects.len() == 1 { "it" } else { "they" },
                if projects.len() == 1 { "s" } else { "" },
            ));
        }
        Self {
            single,
            projects,
            text,
        }
    }

    /// The banner a run starts with: the projects of several tasks, or a
    /// single task's dependencies; nothing for a single task alone.
    pub fn start(&self, paint: Paint) -> String {
        if self.projects.is_empty() || (self.single && !self.text.contains(" and ")) {
            return String::new();
        }
        let mut text = banner(paint, "36", &format!("Running {}:", self.text));
        if !self.single {
            text.push('\n');
            for project in &self.projects {
                text.push_str(&format!("{} {project}\n", paint.dim("-")));
            }
        }
        text
    }
}

/// Nx's banner: ` QK ` reversed, then the title, in one colour.
fn banner(paint: Paint, colour: &str, title: &str) -> String {
    format!(
        "\n{}  {}\n",
        paint.wrap(&format!("7;1;{colour}"), " QK "),
        paint.wrap(colour, title)
    )
}

#[derive(Default)]
struct Progress {
    total: usize,
    /// Running tasks with when they started.
    running: BTreeMap<String, Instant>,
    cached: BTreeSet<String>,
    from_cache: usize,
    ran: usize,
    failed: usize,
    skipped: usize,
    /// Lines the panel occupies, to redraw it in place.
    drawn: usize,
    frame: usize,
}

/// The task a run-one panel hands the terminal to, and its header.
struct Requested {
    id: String,
    header: String,
}

/// A live panel on stderr: counts, then the running tasks with their time.
/// For one requested task it shows only its dependencies, prints warnings as
/// they come, and gives way to the task's output once the task starts.
pub struct Dynamic {
    progress: Mutex<Progress>,
    warnings: Warnings,
    started: Instant,
    paint: Paint,
    done: AtomicBool,
    requested: Option<Requested>,
}

impl Dynamic {
    pub fn start() -> Arc<Self> {
        Self::spawn(None)
    }

    /// The panel for one requested task, `id` as the graph names it and
    /// `printable` as its header does.
    pub fn run_one(id: &str, printable: &str) -> Arc<Self> {
        Self::spawn(Some(Requested {
            id: id.to_owned(),
            header: format!("> qk run {printable}"),
        }))
    }

    fn spawn(requested: Option<Requested>) -> Arc<Self> {
        let dynamic = Arc::new(Self {
            progress: Mutex::default(),
            warnings: Warnings::default(),
            started: Instant::now(),
            paint: Paint::stderr(),
            done: AtomicBool::new(false),
            requested,
        });
        let ticker = dynamic.clone();
        std::thread::spawn(move || {
            while !ticker.done.load(Ordering::Relaxed) {
                ticker.redraw(None);
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        dynamic
    }

    /// Whether warnings print as they come rather than in the summary.
    pub fn prints_warnings(&self) -> bool {
        self.requested.is_some()
    }

    /// Clears the panel for good and hands the terminal to the requested
    /// task: how its dependencies went, then its header. A task with no
    /// dependencies starts with neither, as it does without the panel.
    fn hand_off(&self, progress: &mut Progress, header: &str) {
        self.done.store(true, Ordering::Relaxed);
        let paint = self.paint;
        let mut text = clear(progress.drawn);
        progress.drawn = 0;
        let dependencies = progress.total.saturating_sub(1);
        if dependencies > 0 {
            let mut parts = vec![
                format!("{} from cache", progress.from_cache),
                format!("{} ran", progress.ran),
            ];
            // Continuous dependencies keep running beside the task.
            let serving = progress.running.len();
            if serving > 0 {
                parts.push(format!("{serving} running"));
            }
            text.push_str(&format!(
                "{} {} in {}: {}\n\n{}\n\n",
                paint.green("✔"),
                paint.bold(&format!(
                    "{dependencies} dependenc{} ready",
                    if dependencies == 1 { "y" } else { "ies" }
                )),
                seconds(self.started.elapsed()),
                parts.join(", "),
                paint.bold(header),
            ));
        }
        report::write_all(true, text.as_bytes());
    }

    /// Clears the panel for good.
    pub fn finish(&self) {
        self.done.store(true, Ordering::Relaxed);
        let mut progress = self.progress.lock().unwrap();
        let mut stderr = std::io::stderr().lock();
        let _ = stderr.write_all(clear(progress.drawn).as_bytes());
        progress.drawn = 0;
    }

    /// Redraws the panel, first writing `output` above it.
    fn redraw(&self, output: Option<(bool, &[u8])>) {
        if self.done.load(Ordering::Relaxed) {
            if let Some((stderr, bytes)) = output {
                report::write_all(stderr, bytes);
            }
            return;
        }
        let mut progress = self.progress.lock().unwrap();
        // The panel may have finished while this waited for it.
        if self.done.load(Ordering::Relaxed) {
            drop(progress);
            if let Some((stderr, bytes)) = output {
                report::write_all(stderr, bytes);
            }
            return;
        }
        // Nothing to show until the run is planned.
        if progress.total == 0 && output.is_none() {
            return;
        }
        let (columns, rows) = terminal_size::terminal_size_of(std::io::stderr())
            .map_or((80, 24), |(width, height)| {
                (width.0 as usize, height.0 as usize)
            });
        let mut text = clear(progress.drawn);
        if let Some((stderr, bytes)) = output {
            let _ = std::io::stderr().lock().write_all(text.as_bytes());
            report::write_all(stderr, bytes);
            text.clear();
            // Warnings can come before the run is planned.
            if progress.total == 0 {
                progress.drawn = 0;
                return;
            }
        }
        progress.frame += 1;
        let paint = self.paint;
        let spinner = SPINNER[progress.frame % SPINNER.len()];
        let finished = progress.from_cache + progress.ran + progress.failed + progress.skipped;
        // A run-one panel counts the dependencies, the requested task aside.
        let (total, label) = if self.requested.is_some() {
            (progress.total.saturating_sub(1), "dependencies")
        } else {
            (progress.total, "tasks")
        };
        let queued = total.saturating_sub(finished + progress.running.len());
        let mut lines = vec![format!(
            "{} {} {}  {}  {}  {}{}  {}",
            paint.cyan(spinner),
            paint.bold(&format!("{finished}/{total}")),
            paint.dim(label),
            paint.cyan(&format!("{} running", progress.running.len())),
            paint.dim(&format!("{queued} queued")),
            paint.green(&format!("{} done", progress.ran)),
            paint.dim(&format!(" + {} cached", progress.from_cache)),
            if progress.failed > 0 {
                paint.red(&format!("{} failed", progress.failed))
            } else {
                String::new()
            },
        )];
        lines[0].push_str(&paint.dim(&format!("  {}", seconds(self.started.elapsed()))));
        let mut running: Vec<_> = progress.running.iter().collect();
        running.sort_by_key(|(_, started)| **started);
        let room = rows.saturating_sub(4).clamp(1, 12);
        let width = running
            .iter()
            .map(|(id, _)| id.len())
            .max()
            .unwrap_or(0)
            .min(60);
        for (id, started) in running.iter().take(room) {
            lines.push(format!(
                "  {} {id:width$}  {}",
                paint.dim("·"),
                paint.dim(&seconds(started.elapsed()))
            ));
        }
        if running.len() > room {
            lines.push(paint.dim(&format!("  … {} more", running.len() - room)));
        }
        for line in &lines {
            text.push_str(&truncate(line, columns.saturating_sub(1)));
            text.push('\n');
        }
        progress.drawn = lines.len();
        let _ = std::io::stderr().lock().write_all(text.as_bytes());
    }
}

impl Sink for Dynamic {
    fn event(&self, event: &Event) {
        let mut progress = self.progress.lock().unwrap();
        match event {
            Event::Planned { tasks } => progress.total = *tasks,
            Event::Started { id, .. } | Event::StartedContinuous { id }
                if self
                    .requested
                    .as_ref()
                    .is_some_and(|requested| requested.id == *id) =>
            {
                if !self.done.load(Ordering::Relaxed) {
                    let header = &self.requested.as_ref().unwrap().header;
                    self.hand_off(&mut progress, header);
                }
            }
            Event::Started { id, .. } | Event::StartedContinuous { id } => {
                progress.running.insert(id.clone(), Instant::now());
            }
            Event::Cache {
                id,
                cached: Cached::Hit | Cached::RemoteHit,
            } => {
                progress.cached.insert(id.clone());
            }
            Event::Finished { id, outcome } => {
                progress.running.remove(id);
                match outcome {
                    Outcome::Success if progress.cached.contains(id) => progress.from_cache += 1,
                    Outcome::Success => progress.ran += 1,
                    Outcome::Failed(_) | Outcome::Cancelled => progress.failed += 1,
                }
            }
            Event::Stopped { id } => {
                if progress.running.remove(id).is_some() {
                    progress.ran += 1;
                }
            }
            Event::Skipped { .. } => progress.skipped += 1,
            Event::Cache { .. } | Event::Stopping { .. } => {}
        }
    }

    fn warning(&self, line: &str) {
        if self.prints_warnings() {
            let line = format!("{}\n", self.paint.dim(line));
            self.redraw(Some((true, line.as_bytes())));
        } else {
            self.warnings.push(line);
        }
    }

    fn output(&self, stderr: bool, bytes: &[u8]) {
        self.redraw(Some((stderr, bytes)));
    }
}

/// Moves up over `lines` drawn lines and clears to the end of the screen.
fn clear(lines: usize) -> String {
    if lines == 0 {
        String::new()
    } else {
        format!("\x1b[{lines}A\r\x1b[J")
    }
}

/// Cuts a line to `columns` visible characters, keeping escape sequences.
fn truncate(line: &str, columns: usize) -> String {
    let mut result = String::new();
    let mut visible = 0;
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            result.push(ch);
            for next in chars.by_ref() {
                result.push(next);
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        if visible == columns {
            result.push_str("\x1b[0m");
            break;
        }
        result.push(ch);
        visible += 1;
    }
    result
}

fn seconds(duration: Duration) -> String {
    let seconds = duration.as_secs_f64();
    if seconds < 60.0 {
        format!("{seconds:.1}s")
    } else {
        format!(
            "{}m{:02}s",
            duration.as_secs() / 60,
            duration.as_secs() % 60
        )
    }
}

fn since(later: SystemTime, earlier: SystemTime) -> Duration {
    later.duration_since(earlier).unwrap_or_default()
}

/// The lines a run ends with: the result, the critical path, and whether a
/// higher `--parallel` could have helped, judged by how busy the machine was
/// while ready tasks waited for a slot.
/// What the sandbox found. Paths read by many tasks, at least a quarter of
/// those with findings, come first, once: the
/// package manager and version manager a command starts through read them,
/// not the task. Packages read by as many come next, once, each with how many
/// tasks read it. Then each task's own: the files it read that its key leaves
/// out, the `.git` and dotenv files it read, the installed packages it read
/// that its key leaves out, and what it wrote outside its outputs, up to a
/// few entries each. Tasks restored from the cache did not run, so they are
/// counted apart.
pub fn sandbox_summary(sandbox: &qk_runner::SandboxResult, tasks: usize, paint: Paint) -> String {
    use std::collections::{BTreeMap, BTreeSet};
    const SHOWN: usize = 8;
    let mode = match sandbox.mode {
        qk_runner::sandbox::Mode::Audit => "audit",
        qk_runner::sandbox::Mode::Enforce => "enforce",
    };
    let mut readers: BTreeMap<&str, usize> = BTreeMap::new();
    for findings in sandbox.findings.values() {
        for path in &findings.undeclared_reads {
            *readers.entry(path.as_str()).or_insert(0) += 1;
        }
    }
    let many = |count: usize| count >= 3 && count * 4 >= sandbox.findings.len();
    let shared: BTreeSet<&str> = readers
        .iter()
        .filter(|(_, count)| many(**count))
        .map(|(path, _)| *path)
        .collect();
    // Packages read by many tasks, of those naming their external
    // dependencies and of those found through hoisting.
    let readers_of = |kind: fn(&qk_runner::sandbox::Findings) -> &BTreeSet<String>| {
        let mut readers: BTreeMap<&str, usize> = BTreeMap::new();
        for findings in sandbox.findings.values() {
            for package in kind(findings) {
                *readers.entry(package.as_str()).or_insert(0) += 1;
            }
        }
        readers.retain(|_, count| many(*count));
        readers
    };
    let package_readers = readers_of(|findings| &findings.unkeyed_packages);
    let hoisted_readers = readers_of(|findings| &findings.hoisted_packages);
    let mut lines = Vec::new();
    let restored = sandbox.restored.len();
    // Refused tasks did not run, so none was kept to anything.
    let tasks = tasks.saturating_sub(restored + sandbox.refused.len());
    type Own<'a> = (
        &'a String,
        &'a qk_runner::sandbox::Findings,
        Vec<&'a String>,
        Vec<&'a String>,
        Vec<&'a String>,
    );
    let own: Vec<Own> = sandbox
        .findings
        .iter()
        .map(|(id, findings)| {
            let reads = findings
                .undeclared_reads
                .iter()
                .filter(|path| !shared.contains(path.as_str()))
                .collect::<Vec<_>>();
            let packages = findings
                .unkeyed_packages
                .iter()
                .filter(|package| !package_readers.contains_key(package.as_str()))
                .collect::<Vec<_>>();
            let hoisted = findings
                .hoisted_packages
                .iter()
                .filter(|package| !hoisted_readers.contains_key(package.as_str()))
                .collect::<Vec<_>>();
            (id, findings, reads, packages, hoisted)
        })
        .filter(|(_, findings, reads, packages, hoisted)| {
            !reads.is_empty()
                || !packages.is_empty()
                || !hoisted.is_empty()
                || !findings.tsconfig_reads.is_empty()
                || !findings.dependency_reads.is_empty()
                || !findings.unkeyed_reads.is_empty()
                || !findings.stray_writes.is_empty()
        })
        .collect();
    if tasks == 0 {
        lines.push(format!("Sandbox {mode}: no task ran"));
    } else if sandbox.findings.is_empty() {
        lines.push(format!(
            "Sandbox {mode}: {tasks} task{} kept to what {} declare{}",
            if tasks == 1 { "" } else { "s" },
            if tasks == 1 { "it" } else { "they" },
            if tasks == 1 { "s" } else { "" },
        ));
    } else {
        lines.push(format!(
            "Sandbox {mode}: {} of {tasks} tasks went beyond what they declare",
            sandbox.findings.len()
        ));
    }
    let list = |label: &str, paths: &[&String], lines: &mut Vec<String>, indent: &str| {
        if paths.is_empty() {
            return;
        }
        lines.push(format!("{indent}{} {label}:", paths.len()));
        for path in paths.iter().take(SHOWN) {
            lines.push(format!("{indent}  {path}"));
        }
        if paths.len() > SHOWN {
            lines.push(paint.dim(&format!("{indent}  … and {} more", paths.len() - SHOWN)));
        }
    };
    // Package managers and bundlers read every workspace package's manifest
    // to resolve them, and version managers their configuration, as a
    // command starts: counted once rather than listed for each task.
    for (label, group) in [
        (
            "workspace package.json read outside the keys, as package managers and bundlers resolve workspace packages",
            (|findings| &findings.manifest_reads)
                as fn(&qk_runner::sandbox::Findings) -> &BTreeSet<String>,
        ),
        (
            "toolchain configuration read outside the keys, as commands start",
            |findings| &findings.toolchain_reads,
        ),
    ] {
        let readers = sandbox
            .findings
            .values()
            .filter(|findings| !group(findings).is_empty())
            .count();
        let paths: BTreeSet<&String> = sandbox.findings.values().flat_map(group).collect();
        if paths.is_empty() {
            continue;
        }
        lines.push(paint.dim(&format!(
            "  {} {label}, by {readers} task{}",
            paths.len(),
            if readers == 1 { "" } else { "s" },
        )));
    }
    if !shared.is_empty() {
        let paths: Vec<&String> = sandbox
            .findings
            .values()
            .flat_map(|findings| &findings.undeclared_reads)
            .filter(|path| shared.contains(path.as_str()))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        list(
            "read by many tasks, likely by the tools their commands start through",
            &paths,
            &mut lines,
            "  ",
        );
    }
    let counted = |readers: &BTreeMap<&str, usize>| -> Vec<String> {
        readers
            .iter()
            .map(|(package, tasks)| format!("{package} ({tasks} tasks)"))
            .collect()
    };
    let packages = counted(&package_readers);
    list(
        &format!(
            "installed package{} read by many tasks, not in their keys; name {} in each one's externalDependencies",
            if packages.len() == 1 { "" } else { "s" },
            if packages.len() == 1 { "it" } else { "them" },
        ),
        &packages.iter().collect::<Vec<_>>(),
        &mut lines,
        "  ",
    );
    let hoisted = counted(&hoisted_readers);
    list(
        &hoisted_label(true, hoisted.len()),
        &hoisted.iter().collect::<Vec<_>>(),
        &mut lines,
        "  ",
    );
    for (id, findings, reads, packages, hoisted) in own {
        lines.push(format!("  {id}"));
        list("read outside its inputs", &reads, &mut lines, "    ");
        // Each project's files are named in the report; here, how many.
        if !findings.dependency_reads.is_empty() {
            let projects: Vec<String> = findings
                .dependency_reads
                .iter()
                .map(|(project, paths)| {
                    format!(
                        "{project} ({} file{})",
                        paths.len(),
                        if paths.len() == 1 { "" } else { "s" }
                    )
                })
                .collect();
            let shown = projects.len().min(SHOWN);
            let more = if projects.len() > shown {
                format!(", and {} more", projects.len() - shown)
            } else {
                String::new()
            };
            lines.push(format!(
                "    read in {} project{} it depends on, outside its inputs; ^ inputs key them: {}{more}",
                projects.len(),
                if projects.len() == 1 { "" } else { "s" },
                projects[..shown].join(", "),
            ));
        }
        list(
            "TypeScript configuration read outside its inputs, likely inputs",
            &findings.tsconfig_reads.iter().collect::<Vec<_>>(),
            &mut lines,
            "    ",
        );
        list(
            "read, not in any key",
            &findings.unkeyed_reads.iter().collect::<Vec<_>>(),
            &mut lines,
            "    ",
        );
        list(
            &format!(
                "installed package{} read, not in its key; name {} in its externalDependencies",
                if packages.len() == 1 { "" } else { "s" },
                if packages.len() == 1 { "it" } else { "them" },
            ),
            &packages,
            &mut lines,
            "    ",
        );
        list(
            &hoisted_label(false, hoisted.len()),
            &hoisted,
            &mut lines,
            "    ",
        );
        list(
            "written outside its outputs",
            &findings.stray_writes.iter().collect::<Vec<_>>(),
            &mut lines,
            "    ",
        );
    }
    for (id, reason) in &sandbox.unsandboxed {
        lines.push(paint.dim(&format!("  {id} ran unsandboxed: {reason}")));
    }
    for (id, reason) in &sandbox.refused {
        lines.push(format!(
            "  {id} failed, as it cannot be sandboxed: {reason}"
        ));
    }
    if restored > 0 {
        lines.push(paint.dim(&format!(
            "  {restored} task{} restored from the cache, so not {}",
            if restored == 1 { "" } else { "s" },
            match sandbox.mode {
                qk_runner::sandbox::Mode::Audit => "audited",
                qk_runner::sandbox::Mode::Enforce => "sandboxed",
            },
        )));
    }
    // Many tasks of a large store may share this; the report has each
    // reason.
    if !sandbox.packages_unchecked.is_empty() {
        let ids: Vec<&str> = sandbox
            .packages_unchecked
            .keys()
            .map(String::as_str)
            .collect();
        let shown = ids.len().min(SHOWN);
        let more = if ids.len() > shown {
            format!(" and {} more", ids.len() - shown)
        } else {
            String::new()
        };
        lines.push(paint.dim(&format!(
            "  {} without checking installed packages, as the sandbox rejected {} profile{} naming them: {}{more}",
            if ids.len() == 1 { "1 task ran".to_owned() } else { format!("{} tasks ran", ids.len()) },
            if ids.len() == 1 { "its" } else { "their" },
            if ids.len() == 1 { "" } else { "s" },
            ids[..shown].join(", "),
        )));
    }
    lines.join("\n") + "\n"
}

/// What a list of installed packages the key leaves out although it counts
/// every installed package of its importers says: none of those depends on
/// them, so a package found them through pnpm's hoisting.
fn hoisted_label(by_many: bool, count: usize) -> String {
    format!(
        "installed package{} read {}that nothing {} importers install depends on, found through pnpm's hoisting; declare {} as a dependency of the package importing it",
        if count == 1 { "" } else { "s" },
        if by_many { "by many tasks, " } else { "" },
        if by_many { "their" } else { "its" },
        if count == 1 { "it" } else { "each" },
    )
}

pub fn summary(
    result: &RunResult,
    report: &RunReport,
    parallel: usize,
    cores: usize,
    warnings: &[String],
    paint: Paint,
) -> String {
    let wall = Duration::from_millis(report.ended.saturating_sub(report.started));
    let mut lines = Vec::new();
    let failed: Vec<&str> = report
        .tasks
        .iter()
        .filter(|task| task.status == "failure" || task.status == "cancelled")
        .map(|task| task.id.as_str())
        .collect();
    let skipped_names: Vec<&str> = report
        .tasks
        .iter()
        .filter(|task| task.status == "skipped")
        .map(|task| task.id.as_str())
        .collect();
    let skipped = skipped_names.len();
    let cached = report
        .tasks
        .iter()
        .filter(|task| matches!(task.cache.as_deref(), Some("local-hit" | "remote-hit")))
        .count();
    let total = report.tasks.len();
    if failed.is_empty() {
        lines.push(format!(
            "{} {} in {}: {cached} from cache, {} ran",
            paint.green("✔"),
            paint.bold(&format!("{total} tasks succeeded")),
            seconds(wall),
            total - cached - skipped,
        ));
    } else {
        lines.push(format!(
            "{} {} in {}: {}",
            paint.red("✖"),
            paint.bold(&format!("{} of {total} tasks failed", failed.len())),
            seconds(wall),
            list(&failed),
        ));
        if skipped > 0 {
            lines.push(format!(
                "  {} {}",
                paint.dim(&format!("{skipped} skipped, since a dependency failed:")),
                list(&skipped_names)
            ));
        }
    }
    lines.extend(details(result, report, parallel, cores, warnings, paint));
    lines.join("\n") + "\n"
}

/// Up to eight names, then how many more.
fn list(names: &[&str]) -> String {
    let mut list = names.iter().take(8).copied().collect::<Vec<_>>().join(", ");
    if names.len() > 8 {
        list.push_str(&format!(" and {} more", names.len() - 8));
    }
    list
}

/// What follows a run's result: tasks that started from warm state, thread
/// shares, the critical path and what more parallelism could do, then the
/// warnings kept for the end.
fn details(
    result: &RunResult,
    report: &RunReport,
    parallel: usize,
    cores: usize,
    warnings: &[String],
    paint: Paint,
) -> Vec<String> {
    let wall = Duration::from_millis(report.ended.saturating_sub(report.started));
    let total = report.tasks.len();
    let mut lines = Vec::new();
    let warm: Vec<&str> = report
        .tasks
        .iter()
        .filter(|task| {
            task.warm
                .as_ref()
                .is_some_and(|warm| !crate::history::restored(warm).is_empty())
        })
        .map(|task| task.id.as_str())
        .collect();
    if !warm.is_empty() {
        lines.push(format!(
            "{} {}",
            paint.dim(&format!(
                "{} task{} started from warm state:",
                warm.len(),
                if warm.len() == 1 { "" } else { "s" }
            )),
            list(&warm)
        ));
    }
    // Tasks with `qk:ultracache`, by how they were keyed.
    let recorded = |kinds: &[&str]| -> Vec<&str> {
        report
            .tasks
            .iter()
            .filter(|task| {
                task.recorded
                    .as_ref()
                    .and_then(|recorded| recorded["kind"].as_str())
                    .is_some_and(|kind| kinds.contains(&kind))
            })
            .map(|task| task.id.as_str())
            .collect()
    };
    for (kinds, label) in [
        (
            &["hit"][..],
            "from the cache by what they read when recorded",
        ),
        (&["saved"], "recorded"),
        (
            &["declared"],
            "from the cache by their inputs, which cover what they read when recorded",
        ),
        (
            &["missed"],
            "missed what they read when recorded, and ran unrecorded",
        ),
        (
            &["unrecorded"],
            "with qk:ultracache, keyed by their inputs until recorded",
        ),
        (
            &["failed"],
            "not recorded, as a recording could not be kept; the run says why",
        ),
    ] {
        let tasks = recorded(kinds);
        if !tasks.is_empty() {
            lines.push(format!(
                "{} {}",
                paint.dim(&format!(
                    "{} task{} {label}:",
                    tasks.len(),
                    if tasks.len() == 1 { "" } else { "s" }
                )),
                list(&tasks)
            ));
        }
    }
    // Tasks with `qk:ultracache` mode "record", where what their recordings
    // would have done differs from what their declared inputs did.
    let shadowed = |kind: &str, hit: bool| -> Vec<&str> {
        report
            .tasks
            .iter()
            .filter(|task| {
                task.shadow
                    .as_ref()
                    .and_then(|shadow| shadow["kind"].as_str())
                    == Some(kind)
                    && matches!(task.cache.as_deref(), Some("local-hit" | "remote-hit")) == hit
            })
            .map(|task| task.id.as_str())
            .collect()
    };
    for (tasks, label) in [
        (
            shadowed("wouldHit", false),
            "missed by their inputs, where what they read when recorded had a result",
        ),
        (
            shadowed("wouldMiss", true),
            "from the cache by their inputs, where what they read when recorded had none",
        ),
    ] {
        if !tasks.is_empty() {
            lines.push(format!(
                "{} {}",
                paint.dim(&format!(
                    "{} task{} {label}:",
                    tasks.len(),
                    if tasks.len() == 1 { "" } else { "s" }
                )),
                list(&tasks)
            ));
        }
    }
    // Tasks that ran where a recording would have taken a result from the
    // cache, and gave another: a recording that left out something the task
    // read, or a flaky task.
    for task in &report.tasks {
        let Some(check) = task.shadow.as_ref().map(|shadow| &shadow["check"]) else {
            continue;
        };
        if check["matched"] == serde_json::Value::Bool(false) {
            let differences: Vec<&str> = check["differences"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(serde_json::Value::as_str)
                .collect();
            lines.push(format!(
                "{} {}",
                paint.dim(&format!(
                    "{} gave another result than the one its recording would have taken:",
                    task.id
                )),
                differences.join(", ")
            ));
        }
    }
    let threaded: Vec<String> = report
        .tasks
        .iter()
        .filter_map(|task| {
            let threads = task.threads?;
            let mut line = format!("{} {threads}", task.id);
            if let Some(cores) = task.cores.filter(|cores| *cores != threads) {
                line.push_str(&format!(" on {cores}"));
            }
            if let Some(busy) = task.busy() {
                line.push_str(&format!(" ({busy:.1} busy)"));
            }
            Some(line)
        })
        .collect();
    if !threaded.is_empty() {
        lines.push(format!(
            "{} {}",
            paint.dim(&format!("Threads of {cores} cores:")),
            threaded.join(", ")
        ));
        if result.unexpected {
            lines.push(format!(
                "  {}",
                paint.dim(
                    "none had run before, here or in the remote store, so each was given an even split"
                )
            ));
        }
    }
    if let Some(memory) = result.memory {
        let mut tasks: Vec<(&str, u64)> = report
            .tasks
            .iter()
            .filter_map(|task| Some((task.id.as_str(), task.memory?)))
            .collect();
        tasks.sort_by_key(|(_, memory)| std::cmp::Reverse(*memory));
        let most: Vec<String> = tasks
            .iter()
            .take(3)
            .map(|(id, memory)| format!("{id} {}", crate::history::bytes(*memory)))
            .collect();
        lines.push(format!(
            "{} {}",
            paint.dim(&format!(
                "Memory, at most {} together:",
                crate::history::bytes(memory)
            )),
            most.join(", ")
        ));
    }
    let waited: Vec<&str> = result
        .tasks
        .iter()
        .filter(|(_, task)| !task.memory_wait.is_zero())
        .map(|(id, _)| id.as_str())
        .collect();
    if !waited.is_empty() {
        lines.push(format!(
            "{} {}",
            paint.dim(&format!(
                "{} task{} waited for free memory:",
                waited.len(),
                if waited.len() == 1 { "" } else { "s" }
            )),
            list(&waited)
        ));
    }
    let path = &report.critical_path;
    if path.tasks.len() > 1 || (path.tasks.len() == 1 && total > 1) {
        let by_id: BTreeMap<&str, &qk_history::TaskReport> = report
            .tasks
            .iter()
            .map(|task| (task.id.as_str(), task))
            .collect();
        let arrow = paint.dim(" → ");
        let chain = if path.tasks.len() > 6 {
            format!(
                "{}{arrow}{}{arrow}{}",
                path.tasks[..2].join(&arrow),
                paint.dim(&format!("… {} more", path.tasks.len() - 4)),
                path.tasks[path.tasks.len() - 2..].join(&arrow)
            )
        } else {
            path.tasks.join(&arrow)
        };
        lines.push(format!(
            "{} {} {}: {chain}",
            paint.dim("Critical path"),
            seconds(Duration::from_millis(path.duration)),
            paint.dim(&format!(
                "({} task{})",
                path.tasks.len(),
                if path.tasks.len() == 1 { "" } else { "s" }
            )),
        ));
        let mut longest: Vec<(&str, u64)> = path
            .tasks
            .iter()
            .map(|id| (id.as_str(), by_id[id.as_str()].duration()))
            .collect();
        // The three longest, in the order they ran.
        let mut by_duration = longest.clone();
        by_duration.sort_by_key(|(_, duration)| std::cmp::Reverse(*duration));
        let kept: BTreeSet<&str> = by_duration
            .iter()
            .filter(|(_, duration)| *duration >= 50)
            .take(3)
            .map(|(id, _)| *id)
            .collect();
        longest.retain(|(id, _)| kept.contains(id));
        if path.tasks.len() > 1 && !longest.is_empty() {
            lines.push(format!(
                "  {} {}",
                paint.dim("longest:"),
                longest
                    .iter()
                    .take(3)
                    .map(|(id, duration)| format!(
                        "{id} {}",
                        seconds(Duration::from_millis(*duration))
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        let waited = |id: &str| {
            result
                .tasks
                .get(id)
                // Waiting for memory is not waiting for a slot.
                .map_or(Duration::ZERO, |task| {
                    since(task.started, task.ready).saturating_sub(task.memory_wait)
                })
        };
        let critical_wait: Duration = path.tasks.iter().map(|id| waited(id)).sum();
        let mean = |samples: Vec<f32>| {
            (!samples.is_empty()).then(|| samples.iter().sum::<f32>() / samples.len() as f32)
        };
        let busy_while_waiting = mean(
            result
                .load
                .iter()
                .filter(|(_, waiting)| *waiting)
                .map(|(load, _)| *load)
                .collect(),
        );
        let busy = mean(result.load.iter().map(|(load, _)| *load).collect());
        let percent = |load: f32| format!("{:.0}%", load * 100.0);
        let verdict = if critical_wait < Duration::from_millis(200)
            || critical_wait.as_secs_f64() < wall.as_secs_f64() * 0.05
        {
            format!(
                "no task on the critical path waited for a slot{}",
                busy.map_or(String::new(), |busy| format!(
                    "; the machine averaged {} CPU",
                    percent(busy)
                ))
            )
        } else {
            match busy_while_waiting {
                Some(load) if load >= 0.85 => format!(
                    "critical-path tasks waited {} for a slot, but the machine was {} busy while tasks waited, so a higher --parallel would not help",
                    seconds(critical_wait),
                    percent(load)
                ),
                Some(load) => format!(
                    "critical-path tasks waited {} for a slot while the machine was only {} busy: a higher --parallel could finish up to {} sooner",
                    seconds(critical_wait),
                    percent(load),
                    seconds(critical_wait)
                ),
                None => format!(
                    "critical-path tasks waited {} for a slot",
                    seconds(critical_wait)
                ),
            }
        };
        lines.push(format!(
            "{} {verdict}",
            paint.dim(&format!("Parallel {parallel}:"))
        ));
    }
    if !warnings.is_empty() {
        lines.push(paint.yellow(&format!(
            "{} warning{}:",
            warnings.len(),
            if warnings.len() == 1 { "" } else { "s" }
        )));
        for warning in warnings.iter().take(10) {
            lines.push(format!("  {}", warning.trim_start_matches("qk: ")));
        }
        if warnings.len() > 10 {
            lines.push(format!("  … {} more", warnings.len() - 10));
        }
    }
    lines
}

/// The banner a static run ends with, as Nx's: what ran, or the tasks that
/// failed and those not run because of them, then the details.
pub fn static_summary(
    title: &Title,
    result: &RunResult,
    report: &RunReport,
    parallel: usize,
    cores: usize,
    paint: Paint,
) -> String {
    if report.tasks.is_empty() {
        return banner(paint, "36", "No tasks were run");
    }
    let wall = Duration::from_millis(report.ended.saturating_sub(report.started));
    let with = |status: &str| -> Vec<&str> {
        report
            .tasks
            .iter()
            .filter(|task| task.status == status)
            .map(|task| task.id.as_str())
            .collect()
    };
    let mut failed = with("failure");
    failed.extend(with("cancelled"));
    let skipped = with("skipped");
    let cached = report
        .tasks
        .iter()
        .filter(|task| matches!(task.cache.as_deref(), Some("local-hit" | "remote-hit")))
        .count();
    let total = report.tasks.len();
    let mut text = if failed.is_empty() {
        banner(paint, "32", &format!("Successfully ran {}", title.text))
    } else {
        banner(paint, "31", &format!("Running {} failed", title.text))
    };
    let mut lines = vec![String::new()];
    if !skipped.is_empty() {
        lines.push(paint.dim("Tasks not run because their dependencies failed:"));
        lines.push(String::new());
        lines.extend(skipped.iter().map(|id| format!("{} {id}", paint.dim("-"))));
        lines.push(String::new());
    }
    if !failed.is_empty() {
        lines.push(paint.dim("Failed tasks:"));
        lines.push(String::new());
        lines.extend(failed.iter().map(|id| {
            let code = match result.outcomes.get(*id) {
                Some(qk_executor::Outcome::Failed(code)) => format!(" (exit {code})"),
                Some(qk_executor::Outcome::Cancelled) => " (cancelled)".to_owned(),
                _ => String::new(),
            };
            format!("{} {id}{}", paint.dim("-"), paint.dim(&code))
        }));
        lines.push(String::new());
    }
    lines.push(paint.dim(&format!(
        "{total} task{} in {}: {cached} from cache, {} ran",
        if total == 1 { "" } else { "s" },
        seconds(wall),
        total - cached - skipped.len() - failed.len(),
    )));
    lines.extend(details(result, report, parallel, cores, &[], paint));
    // As Nx says when `static-failures-only` left output out.
    let collapsed = qk_executor::collapsed();
    if collapsed > 0 {
        lines.push(String::new());
        lines.push(format!(
            "{} --verbose {} --output-style=static {}",
            paint.dim(&format!(
                "Output of {collapsed} successful {} not shown. Run with",
                if collapsed == 1 {
                    "task was"
                } else {
                    "tasks were"
                }
            )),
            paint.dim("or"),
            paint.dim("to see it.")
        ));
    }
    text.push_str(&lines.join("\n"));
    text.push('\n');
    text
}

/// Nx's `summary` style: the counts, then each failed task with the file
/// holding its output, and how to see the logs inline.
pub fn summary_style(
    graph: &qk_taskgraph::TaskGraph,
    report: &RunReport,
    logs: &std::path::Path,
    paint: Paint,
) -> String {
    let with = |status: &'static str| {
        report
            .tasks
            .iter()
            .filter(move |task| task.status == status)
            .collect::<Vec<_>>()
    };
    let failed = with("failure");
    let stopped = with("cancelled");
    let skipped = with("skipped").len();
    let succeeded = report.tasks.len() - failed.len() - stopped.len() - skipped;
    let cached = report
        .tasks
        .iter()
        .filter(|task| matches!(task.cache.as_deref(), Some("local-hit" | "remote-hit")))
        .count();
    let total = report.tasks.len();
    let mut parts = vec![format!("{succeeded} succeeded")];
    if cached > 0 {
        parts.push(format!("{cached} cached"));
    }
    if !failed.is_empty() {
        parts.push(format!("{} failed", failed.len()));
    }
    if !stopped.is_empty() {
        parts.push(format!("{} stopped", stopped.len()));
    }
    if skipped > 0 {
        parts.push(format!("{skipped} skipped"));
    }
    let title = format!(
        "{total} {}: {}",
        if total == 1 { "task" } else { "tasks" },
        parts.join(", ")
    );
    let printable = |id: &str| {
        graph.tasks.get(id).map_or_else(
            || id.to_owned(),
            |task| {
                qk_executor::printable(
                    &task.project,
                    &task.target,
                    task.configuration.as_deref(),
                    &task.args,
                )
            },
        )
    };
    let mut lines = Vec::new();
    for task in &failed {
        lines.push(String::new());
        lines.push(format!("{} qk run {}", paint.red("✖"), printable(&task.id)));
        let log = logs.join(qk_runner::log_name(&task.id));
        if std::fs::metadata(&log).is_ok_and(|metadata| metadata.len() > 0) {
            lines.push(paint.dim(&format!("  full log: {}", log.display())));
        }
    }
    if !stopped.is_empty() {
        lines.push(String::new());
        lines.push(paint.dim("Stopped before finishing:"));
        lines.extend(
            stopped
                .iter()
                .map(|task| format!("{} {}", paint.dim("-"), task.id)),
        );
    }
    lines.push(String::new());
    lines.push(paint.dim("Re-run with --output-style=static to inline logs."));
    let colour = if failed.is_empty() && stopped.is_empty() {
        "32"
    } else {
        "31"
    };
    let mut text = banner(paint, colour, &title);
    text.push_str(&lines.join("\n"));
    text.push('\n');
    text
}

/// The warnings a sink kept for the summary.
pub fn warnings(sink: &SinkKind) -> Vec<String> {
    match sink {
        SinkKind::Lines | SinkKind::Static | SinkKind::Summary => Vec::new(),
        SinkKind::Quiet(quiet) => quiet.warnings.0.lock().unwrap().clone(),
        // A run-one panel printed them as they came.
        SinkKind::Dynamic(dynamic) if dynamic.prints_warnings() => Vec::new(),
        SinkKind::Dynamic(dynamic) => dynamic.warnings.0.lock().unwrap().clone(),
    }
}

/// The sink the CLI installed, kept to finish it and read its warnings.
pub enum SinkKind {
    Lines,
    Static,
    /// Nx's `summary`: counts and the failed tasks, after the run.
    Summary,
    Quiet(Arc<Quiet>),
    Dynamic(Arc<Dynamic>),
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use qk_history::TaskReport;
    use qk_runner::TaskRecord;

    use super::*;

    /// Two tasks, `b` after `a`, where `b` waited `wait` for a slot, with the
    /// machine at `load` while it did.
    fn run(wait: u64, load: f32) -> (RunResult, RunReport) {
        let at = |ms: u64| UNIX_EPOCH + Duration::from_millis(ms);
        let record = |ready, started, ended| TaskRecord {
            ready: at(ready),
            started: at(started),
            ended: at(ended),
            outcome: Outcome::Success,
            cache: qk_cache::CacheStatus::Miss,
            key: None,
            warm: None,
            threads: None,
            cores: None,
            cpu: None,
            memory: None,
            memory_wait: Duration::ZERO,
            execution: None,
            recorded: None,
            shadow: None,
            audit: None,
        };
        let report = |id: &str, started: u64, ended: u64, dependencies: &[&str]| TaskReport {
            id: id.into(),
            project: "app".into(),
            target: id.rsplit(':').next().unwrap().into(),
            configuration: None,
            status: "success".into(),
            cache: Some("miss".into()),
            key: None,
            started: Some(started),
            ended: Some(ended),
            dependencies: dependencies.iter().map(|id| (*id).to_owned()).collect(),
            cause: None,
            warm: None,
            threads: None,
            memory: None,
            recorded: None,
            cores: None,
            cpu: None,
            shadow: None,
            audit: None,
        };
        let result = RunResult {
            sandbox: None,
            input_analysis: None,
            outcomes: BTreeMap::new(),
            skipped: BTreeSet::new(),
            exit_code: 0,
            tasks: BTreeMap::from([
                ("app:a".to_owned(), record(0, 0, 1_000)),
                (
                    "app:b".to_owned(),
                    record(1_000, 1_000 + wait, 2_000 + wait),
                ),
            ]),
            load: vec![(load, true), (load, false)],
            memory: None,
            unexpected: false,
        };
        let tasks = vec![
            report("app:a", 0, 1_000, &[]),
            report("app:b", 1_000 + wait, 2_000 + wait, &["app:a"]),
        ];
        let report = RunReport {
            schema_version: 1,
            id: "run".into(),
            command: vec![],
            sha: None,
            worktree: None,
            started: 0,
            ended: 2_000 + wait,
            exit_code: 0,
            critical_path: qk_history::critical_path(&tasks),
            tasks,
        };
        (result, report)
    }

    fn verdict(wait: u64, load: f32) -> String {
        let (result, report) = run(wait, load);
        let summary = summary(&result, &report, 4, 8, &[], Paint(false));
        summary
            .lines()
            .find(|line| line.starts_with("Parallel"))
            .unwrap()
            .to_owned()
    }

    #[test]
    fn quiet_strips_string_controls_across_writes() {
        let cases: &[(&[u8], &[u8])] = &[
            (b"before\x1b]0;title\x07after", b"beforeafter"),
            (
                b"\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\!",
                b"link!",
            ),
            (b"a\x1bPdata\x07more\x1b\\b", b"ab"),
            (b"a\x1bXdata\x1b\\b", b"ab"),
            (b"a\x1b^data\x1b\\b", b"ab"),
            (b"a\x1b_data\x1b\\b", b"ab"),
            (b"a\x1b]data\x1b\x1b\\b", b"ab"),
            (b"a\x1b]data\x1bxmore\x1b\x07b", b"ab"),
            // Ordinary escapes and UTF-8 continuation bytes stay intact.
            (b"a\x1bZ\xc5\x9cb", b"a\x1bZ\xc5\x9cb"),
        ];
        for &(bytes, expected) in cases {
            for split in 0..=bytes.len() {
                let mut state = EscapeState::default();
                let mut result = state.strip(&bytes[..split]);
                result.extend_from_slice(&state.strip(&bytes[split..]));
                assert_eq!(result, expected, "{bytes:?}, split {split}");
                assert!(!state.finish());
            }
            let mut state = EscapeState::default();
            let result: Vec<_> = bytes
                .iter()
                .flat_map(|byte| state.strip(std::slice::from_ref(byte)))
                .collect();
            assert_eq!(result, expected);
            assert!(!state.finish());
        }
    }

    #[test]
    fn quiet_discards_unfinished_string_controls_before_the_next_task() {
        for introducer in b"]PX^_" {
            for trailing_escape in [false, true] {
                let mut state = EscapeState::default();
                let mut bytes = vec![0x1b, *introducer];
                bytes.extend_from_slice(b"unfinished");
                if trailing_escape {
                    bytes.push(0x1b);
                }
                assert!(state.strip(&bytes).is_empty());
                assert!(!state.finish());
                assert_eq!(state.strip(b"> qk run next"), b"> qk run next");
            }
        }
    }

    #[test]
    fn quiet_strips_csi_with_punctuation_final_bytes() {
        for final_byte in b"@[\\]^_`{|}~" {
            let mut bytes = b"\x1b[1".to_vec();
            bytes.push(*final_byte);
            bytes.extend_from_slice(b"12345\n");
            for split in 0..=bytes.len() {
                let mut state = EscapeState::default();
                let mut result = state.strip(&bytes[..split]);
                result.extend_from_slice(&state.strip(&bytes[split..]));
                assert_eq!(
                    result, b"12345\n",
                    "final byte {final_byte:#x}, split {split}"
                );
                assert!(!state.finish());
            }
        }
    }

    #[test]
    fn quiet_strips_escapes_split_between_replay_chunks() {
        for split in 0..=14 {
            let bytes = b"a\x1b[31mred\x1b[0m!";
            let mut state = EscapeState::default();
            let mut result = state.strip(&bytes[..split]);
            result.extend_from_slice(&state.strip(&bytes[split..]));
            assert_eq!(result, b"ared!");
            assert!(!state.finish());
        }
        let mut state = EscapeState::default();
        assert_eq!(state.strip(b"one\x1b["), b"one");
        assert!(!state.finish());
        assert_eq!(state.strip(b"> qk run next"), b"> qk run next");
        assert!(state.strip(b"\x1b").is_empty());
        assert!(state.finish());
    }

    #[test]
    fn suggests_more_parallelism_only_when_the_machine_had_room() {
        assert!(verdict(0, 0.3).contains("no task on the critical path waited"));
        let idle = verdict(1_500, 0.3);
        assert!(
            idle.contains("a higher --parallel could finish up to 1.5s sooner"),
            "{idle}"
        );
        let busy = verdict(1_500, 0.95);
        assert!(
            busy.contains("95% busy") && busy.contains("would not help"),
            "{busy}"
        );
    }

    #[test]
    fn a_wait_for_memory_is_not_a_wait_for_a_slot() {
        let (mut result, report) = run(1_500, 0.3);
        result.tasks.get_mut("app:b").unwrap().memory_wait = Duration::from_millis(1_500);
        let text = summary(&result, &report, 4, 8, &[], Paint(false));
        assert!(
            text.contains("no task on the critical path waited for a slot"),
            "{text}"
        );
        assert!(
            text.contains("1 task waited for free memory: app:b"),
            "{text}"
        );
    }

    #[test]
    fn names_the_tasks_that_started_from_warm_state() {
        let (result, mut report) = run(0, 0.3);
        assert!(!summary(&result, &report, 4, 8, &[], Paint(false)).contains("warm"));
        report.tasks[1].warm = Some(serde_json::json!({
            "restored": {"source": "remote main", "groups": 1, "files": 3, "bytes": 10}
        }));
        let text = summary(&result, &report, 4, 8, &[], Paint(false));
        assert!(
            text.contains("1 task started from warm state: app:b"),
            "{text}"
        );
    }

    #[test]
    fn lists_a_package_read_by_many_tasks_once() {
        let findings = |packages: &[&str]| qk_runner::sandbox::Findings {
            unkeyed_packages: packages.iter().map(|package| package.to_string()).collect(),
            ..Default::default()
        };
        let sandbox = qk_runner::SandboxResult {
            mode: qk_runner::sandbox::Mode::Audit,
            findings: [
                ("a:lint", findings(&["eslint", "fallow"])),
                ("b:lint", findings(&["eslint"])),
                ("c:lint", findings(&["eslint"])),
            ]
            .into_iter()
            .map(|(id, findings)| (id.to_owned(), findings))
            .collect(),
            unsandboxed: BTreeMap::new(),
            refused: BTreeMap::new(),
            packages_unchecked: BTreeMap::new(),
            audited: BTreeSet::new(),
            restored: BTreeSet::new(),
        };
        let text = sandbox_summary(&sandbox, 3, Paint(false));
        assert_eq!(
            text,
            "Sandbox audit: 3 of 3 tasks went beyond what they declare
  1 installed package read by many tasks, not in their keys; name it in each one's externalDependencies:
    eslint (3 tasks)
  a:lint
    1 installed package read, not in its key; name it in its externalDependencies:
      fallow
"
        );
    }

    #[test]
    fn counts_manifest_and_toolchain_reads_once_and_lists_tsconfigs() {
        let findings = |manifests: &[&str], tsconfigs: &[&str], reads: &[&str]| {
            let set = |paths: &[&str]| paths.iter().map(|path| path.to_string()).collect();
            qk_runner::sandbox::Findings {
                manifest_reads: set(manifests),
                toolchain_reads: set(&[".tool-versions"]),
                tsconfig_reads: set(tsconfigs),
                undeclared_reads: set(reads),
                ..Default::default()
            }
        };
        let sandbox = qk_runner::SandboxResult {
            mode: qk_runner::sandbox::Mode::Audit,
            findings: [
                (
                    "a:build",
                    findings(
                        &["packages/a/package.json", "packages/b/package.json"],
                        &[],
                        &[],
                    ),
                ),
                (
                    "b:build",
                    findings(
                        &["packages/a/package.json"],
                        &["packages/b/tsconfig.lib.json"],
                        &["notes.txt"],
                    ),
                ),
            ]
            .into_iter()
            .map(|(id, findings)| (id.to_owned(), findings))
            .collect(),
            unsandboxed: BTreeMap::new(),
            refused: BTreeMap::new(),
            packages_unchecked: BTreeMap::new(),
            audited: BTreeSet::new(),
            restored: BTreeSet::new(),
        };
        assert_eq!(
            sandbox_summary(&sandbox, 2, Paint(false)),
            "Sandbox audit: 2 of 2 tasks went beyond what they declare
  2 workspace package.json read outside the keys, as package managers and bundlers resolve workspace packages, by 2 tasks
  1 toolchain configuration read outside the keys, as commands start, by 2 tasks
  b:build
    1 read outside its inputs:
      notes.txt
    1 TypeScript configuration read outside its inputs, likely inputs:
      packages/b/tsconfig.lib.json
"
        );
    }

    #[test]
    fn counts_the_tasks_whose_packages_went_unchecked_on_one_line() {
        let sandbox = qk_runner::SandboxResult {
            mode: qk_runner::sandbox::Mode::Audit,
            findings: BTreeMap::new(),
            unsandboxed: BTreeMap::new(),
            refused: BTreeMap::new(),
            packages_unchecked: (0..10)
                .map(|index| (format!("lib{index}:lint"), "too long".to_owned()))
                .collect(),
            audited: BTreeSet::new(),
            restored: BTreeSet::new(),
        };
        let text = sandbox_summary(&sandbox, 10, Paint(false));
        assert_eq!(
            text,
            "Sandbox audit: 10 tasks kept to what they declare
  10 tasks ran without checking installed packages, as the sandbox rejected their profiles naming them: lib0:lint, lib1:lint, lib2:lint, lib3:lint, lib4:lint, lib5:lint, lib6:lint, lib7:lint and 2 more
"
        );
    }

    #[test]
    fn counts_the_tasks_restored_from_the_cache_apart() {
        let sandbox = qk_runner::SandboxResult {
            mode: qk_runner::sandbox::Mode::Audit,
            findings: [(
                "a:build".to_owned(),
                qk_runner::sandbox::Findings {
                    undeclared_reads: ["notes.txt".to_owned()].into(),
                    ..Default::default()
                },
            )]
            .into(),
            unsandboxed: BTreeMap::new(),
            refused: BTreeMap::new(),
            packages_unchecked: BTreeMap::new(),
            audited: ["a:build".to_owned(), "b:build".to_owned()].into(),
            restored: ["c:build".to_owned(), "d:build".to_owned()].into(),
        };
        assert_eq!(
            sandbox_summary(&sandbox, 4, Paint(false)),
            "Sandbox audit: 1 of 2 tasks went beyond what they declare
  a:build
    1 read outside its inputs:
      notes.txt
  2 tasks restored from the cache, so not audited
"
        );
    }
}
