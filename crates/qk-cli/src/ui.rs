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
    let list = |names: &[&str]| {
        let mut list = names.iter().take(8).copied().collect::<Vec<_>>().join(", ");
        if names.len() > 8 {
            list.push_str(&format!(" and {} more", names.len() - 8));
        }
        list
    };
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
            paint.dim(&format!("({} tasks)", path.tasks.len())),
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
    lines.join("\n") + "\n"
}

/// The warnings a sink kept for the summary.
pub fn warnings(sink: &SinkKind) -> Vec<String> {
    match sink {
        SinkKind::Lines => Vec::new(),
        SinkKind::Quiet(quiet) => quiet.warnings.0.lock().unwrap().clone(),
        SinkKind::Dynamic(dynamic) => dynamic.warnings.0.lock().unwrap().clone(),
    }
}

/// The sink the CLI installed, kept to finish it and read its warnings.
pub enum SinkKind {
    Lines,
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
        };
        let result = RunResult {
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
