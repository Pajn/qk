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
    pub fn stderr() -> Self {
        let set = |name| std::env::var_os(name).is_some();
        Self(!set("NO_COLOR") && (set("FORCE_COLOR") || std::io::stderr().is_terminal()))
    }

    /// Colours for a static log, following picocolors as task output does:
    /// also in CI, whose log viewers render them.
    pub fn log() -> Self {
        let set = |name| std::env::var_os(name).is_some();
        Self(
            !set("NO_COLOR")
                && (set("FORCE_COLOR")
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

    fn bold(self, text: &str) -> String {
        self.wrap("1", text)
    }
    fn dim(self, text: &str) -> String {
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
}

impl Default for Quiet {
    fn default() -> Self {
        let plain = |terminal: bool| !terminal && std::env::var_os("FORCE_COLOR").is_none();
        Self {
            warnings: Warnings::default(),
            plain: [
                plain(std::io::stdout().is_terminal()),
                plain(std::io::stderr().is_terminal()),
            ],
        }
    }
}

/// Text without ANSI escape sequences.
fn strip_escapes(bytes: &[u8]) -> Vec<u8> {
    let mut result = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == 0x1b && bytes.get(index + 1) == Some(&b'[') {
            index += 2;
            while index < bytes.len() && !bytes[index].is_ascii_alphabetic() {
                index += 1;
            }
            index += 1;
        } else {
            result.push(bytes[index]);
            index += 1;
        }
    }
    result
}

impl Sink for Quiet {
    fn event(&self, _: &Event) {}

    fn warning(&self, line: &str) {
        self.warnings.push(line);
    }

    fn output(&self, stderr: bool, bytes: &[u8]) {
        if self.plain[usize::from(stderr)] {
            report::write_all(stderr, &strip_escapes(bytes));
        } else {
            report::write_all(stderr, bytes);
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

/// A live panel on stderr: counts, then the running tasks with their time.
pub struct Dynamic {
    progress: Mutex<Progress>,
    warnings: Warnings,
    started: Instant,
    paint: Paint,
    done: AtomicBool,
}

impl Dynamic {
    pub fn start() -> Arc<Self> {
        let dynamic = Arc::new(Self {
            progress: Mutex::default(),
            warnings: Warnings::default(),
            started: Instant::now(),
            paint: Paint::stderr(),
            done: AtomicBool::new(false),
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
        }
        progress.frame += 1;
        let paint = self.paint;
        let spinner = SPINNER[progress.frame % SPINNER.len()];
        let finished = progress.from_cache + progress.ran + progress.failed + progress.skipped;
        let queued = progress
            .total
            .saturating_sub(finished + progress.running.len());
        let mut lines = vec![format!(
            "{} {} {}  {}  {}  {}{}  {}",
            paint.cyan(spinner),
            paint.bold(&format!("{finished}/{}", progress.total)),
            paint.dim("tasks"),
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
        self.warnings.push(line);
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
/// not the task. Then each task's own: the files it read that its key leaves
/// out, the `.git` and dotenv files it read, and what it wrote outside its
/// outputs, up to a few paths each.
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
    let shared: BTreeSet<&str> = readers
        .iter()
        .filter(|(_, count)| **count >= 3 && **count * 4 >= sandbox.findings.len())
        .map(|(path, _)| *path)
        .collect();
    let mut lines = Vec::new();
    let own: Vec<(&String, &qk_runner::sandbox::Findings, Vec<&String>)> = sandbox
        .findings
        .iter()
        .map(|(id, findings)| {
            let reads = findings
                .undeclared_reads
                .iter()
                .filter(|path| !shared.contains(path.as_str()))
                .collect::<Vec<_>>();
            (id, findings, reads)
        })
        .filter(|(_, findings, reads)| {
            !reads.is_empty()
                || !findings.unkeyed_reads.is_empty()
                || !findings.stray_writes.is_empty()
        })
        .collect();
    if sandbox.findings.is_empty() {
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
    for (id, findings, reads) in own {
        lines.push(format!("  {id}"));
        list("read outside its inputs", &reads, &mut lines, "    ");
        list(
            "read, not in any key",
            &findings.unkeyed_reads.iter().collect::<Vec<_>>(),
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
    lines.join("\n") + "\n"
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
                .and_then(|warm| warm.get("restored"))
                .is_some_and(|restored| !restored.is_null())
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
    let threaded: Vec<String> = report
        .tasks
        .iter()
        .filter_map(|task| Some(format!("{} {}", task.id, task.threads?)))
        .collect();
    if !threaded.is_empty() {
        lines.push(format!(
            "{} {}",
            paint.dim(&format!("Threads of {cores} cores:")),
            threaded.join(", ")
        ));
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
                .map_or(Duration::ZERO, |task| since(task.started, task.ready))
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
    text.push_str(&lines.join("\n"));
    text.push('\n');
    text
}

/// The warnings a sink kept for the summary.
pub fn warnings(sink: &SinkKind) -> Vec<String> {
    match sink {
        SinkKind::Lines | SinkKind::Static => Vec::new(),
        SinkKind::Quiet(quiet) => quiet.warnings.0.lock().unwrap().clone(),
        SinkKind::Dynamic(dynamic) => dynamic.warnings.0.lock().unwrap().clone(),
    }
}

/// The sink the CLI installed, kept to finish it and read its warnings.
pub enum SinkKind {
    Lines,
    Static,
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
            memory: None,
            execution: None,
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
}
