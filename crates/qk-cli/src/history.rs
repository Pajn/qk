//! Recording runs and reading them back: `--report`, `qk show runs`,
//! `qk show run` and `qk show task`.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use qk_config::Workspace;
use qk_history::{Cause, History, RunReport, TaskReport};
use qk_runner::RunResult;
use qk_taskgraph::TaskGraph;

/// Where the workspace's history lives: beside its cache, so linked worktrees
/// share it as they share the cache.
pub fn path(workspace: &Workspace) -> PathBuf {
    let cache = qk_cache::cache_directory(&workspace.root);
    cache
        .parent()
        .and_then(Path::parent)
        .map_or_else(|| cache.join("history"), Path::to_path_buf)
        .join("history.db")
}

fn millis(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
}

/// Records a finished run and writes its report where asked. Failures are
/// reported without failing the run.
pub fn record(
    workspace: &Workspace,
    graph: &TaskGraph,
    result: &RunResult,
    started: SystemTime,
    report: Option<&Path>,
) -> RunReport {
    let mut inputs = BTreeMap::new();
    let tasks = graph
        .tasks
        .iter()
        .map(|(id, task)| {
            let record = result.tasks.get(id);
            if let Some((key, value)) = record.and_then(|record| record.key.as_ref()) {
                inputs.insert(key.clone(), value.clone());
            }
            let status = match record.map(|record| record.outcome) {
                Some(qk_executor::Outcome::Success) => "success",
                Some(qk_executor::Outcome::Failed(_)) => "failure",
                Some(qk_executor::Outcome::Cancelled) => "cancelled",
                None => "skipped",
            };
            TaskReport {
                id: id.clone(),
                project: task.project.clone(),
                target: task.target.clone(),
                configuration: task.configuration.clone(),
                status: status.into(),
                cache: record.map(|record| {
                    serde_json::to_value(record.cache)
                        .ok()
                        .and_then(|value| value.as_str().map(str::to_owned))
                        .unwrap_or_default()
                }),
                key: record.and_then(|record| record.key.as_ref().map(|(key, _)| key.clone())),
                started: record.map(|record| millis(record.started)),
                ended: record.map(|record| millis(record.ended)),
                dependencies: task.dependencies.iter().cloned().collect(),
                cause: None,
                warm: record
                    .and_then(|record| record.warm.as_ref())
                    .and_then(|warm| serde_json::to_value(warm).ok()),
                threads: record
                    .and_then(|record| record.threads)
                    .map(|threads| threads as u64),
                memory: record.and_then(|record| record.memory),
            }
        })
        .collect();
    let started = millis(started);
    let run = RunReport {
        schema_version: qk_history::SCHEMA_VERSION,
        id: format!("{started}-{}", std::process::id()),
        command: std::env::args().collect(),
        sha: std::process::Command::new("git")
            .current_dir(&workspace.root)
            .args(["rev-parse", "HEAD"])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .map(|sha| sha.trim().to_owned()),
        started,
        ended: millis(SystemTime::now()),
        exit_code: result.exit_code,
        tasks,
        critical_path: Default::default(),
    };
    let executions = result
        .tasks
        .iter()
        .filter_map(|(id, record)| {
            let execution = record.execution.as_ref()?;
            Some((
                id.as_str(),
                qk_history::Execution {
                    data: &execution.log.data,
                    truncated: execution.log.truncated,
                    inputs_unchanged: execution.inputs_unchanged,
                },
            ))
        })
        .collect();
    let run = match History::open(&path(workspace))
        .and_then(|mut history| history.record_with_executions(run.clone(), &inputs, &executions))
    {
        Ok(run) => run,
        Err(error) => {
            qk_executor::status!("qk: could not record the run: {error:#}");
            RunReport {
                critical_path: qk_history::critical_path(&run.tasks),
                ..run
            }
        }
    };
    if let Some(report) = report {
        let written = serde_json::to_vec_pretty(&run)
            .map_err(anyhow::Error::from)
            .and_then(|mut bytes| {
                bytes.push(b'\n');
                std::fs::write(report, bytes).map_err(anyhow::Error::from)
            });
        if let Err(error) = written {
            qk_executor::status!(
                "qk: could not write the run report to {}: {error:#}",
                report.display()
            );
        }
    }
    run
}

/// What each task is expected to take, from the runs recorded so far; none
/// when there are none or they cannot be read.
pub fn expected(workspace: &Workspace) -> std::collections::BTreeMap<String, qk_runner::Expected> {
    let path = path(workspace);
    if !path.exists() {
        return Default::default();
    }
    History::open(&path)
        .and_then(|history| history.expected())
        .map(|expected| {
            expected
                .into_iter()
                .map(|(id, expected)| {
                    let expected = qk_runner::Expected {
                        millis: expected.millis,
                        threads: expected.threads,
                    };
                    (id, expected)
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn open(workspace: &Workspace) -> Result<History> {
    let path = path(workspace);
    if !path.exists() {
        bail!("no runs recorded yet");
    }
    History::open(&path)
}

pub fn show_runs(
    workspace: &Workspace,
    limit: usize,
    json: bool,
    out: &mut impl Write,
) -> Result<()> {
    let runs = open(workspace)?.runs(limit)?;
    if json {
        serde_json::to_writer_pretty(&mut *out, &runs)?;
        writeln!(out)?;
        return Ok(());
    }
    for run in runs {
        let counts: Vec<String> = run
            .cache
            .iter()
            .map(|(status, count)| format!("{count} {status}"))
            .collect();
        writeln!(
            out,
            "{}  {}  exit {}  {}  {} tasks ({})  {}",
            run.id,
            ago(run.started),
            run.exit_code,
            seconds(run.ended.saturating_sub(run.started)),
            run.tasks,
            counts.join(", "),
            command(&run.command),
        )?;
    }
    Ok(())
}

pub fn show_run(
    workspace: &Workspace,
    id: Option<&str>,
    json: bool,
    out: &mut impl Write,
) -> Result<()> {
    let Some(run) = open(workspace)?.run(id)? else {
        bail!("no run {}", id.unwrap_or("recorded"));
    };
    if json {
        serde_json::to_writer_pretty(&mut *out, &run)?;
        writeln!(out)?;
        return Ok(());
    }
    writeln!(
        out,
        "Run {} {}: {}, exit {}, {}",
        run.id,
        ago(run.started),
        command(&run.command),
        run.exit_code,
        seconds(run.ended.saturating_sub(run.started))
    )?;
    let width = run
        .tasks
        .iter()
        .map(|task| task.id.len())
        .max()
        .unwrap_or(0);
    let mut tasks = run.tasks.clone();
    tasks.sort_by_key(|task| task.started);
    for task in &tasks {
        writeln!(
            out,
            "  {:width$}  {:9}  {:10}  {:>7}  {}",
            task.id,
            task.status,
            task.cache.as_deref().unwrap_or(""),
            seconds(task.duration()),
            match (&task.cause, task.cache.as_deref()) {
                (Some(cause), Some("miss")) if cause.changed == ["unchanged"] => {
                    "same key, but no stored entry".into()
                }
                (cause, _) => cause.as_ref().map(summary).unwrap_or_default(),
            }
        )?;
    }
    if !run.critical_path.tasks.is_empty() {
        writeln!(
            out,
            "Critical path, {}: {}",
            seconds(run.critical_path.duration),
            run.critical_path.tasks.join(" -> ")
        )?;
    }
    Ok(())
}

pub fn show_task(
    workspace: &Workspace,
    task: &str,
    limit: usize,
    json: bool,
    out: &mut impl Write,
) -> Result<()> {
    let records = open(workspace)?.task(task, limit)?;
    if records.is_empty() {
        bail!("no recorded runs of {task}");
    }
    if json {
        let records: Vec<_> = records
            .iter()
            .map(|(run, task)| serde_json::json!({"run": run, "task": task}))
            .collect();
        serde_json::to_writer_pretty(&mut *out, &records)?;
        writeln!(out)?;
        return Ok(());
    }
    writeln!(out, "{task}, most recent first:")?;
    for (run, record) in &records {
        writeln!(
            out,
            "  run {run}  {}  {}  {}  {}",
            record.started.map(ago).unwrap_or_default(),
            record.status,
            record.cache.as_deref().unwrap_or("-"),
            seconds(record.duration()),
        )?;
        if let Some(cause) = &record.cause {
            for line in details(cause) {
                writeln!(out, "      {line}")?;
            }
        }
        if let Some(warm) = record.warm.as_ref().and_then(warm_line) {
            writeln!(out, "      {warm}")?;
        }
        if let Some(threads) = record.threads {
            writeln!(
                out,
                "      ran with {threads} thread{}",
                if threads == 1 { "" } else { "s" }
            )?;
        }
        if let Some(memory) = record.memory {
            writeln!(out, "      used {} of memory at most", bytes(memory))?;
        }
    }
    if let Some(effect) = warm_effect(&records) {
        writeln!(out, "{effect}")?;
    }
    Ok(())
}

/// How long the task took when it ran from warm state and when it ran
/// without, over its successful runs that executed rather than hit the cache.
fn warm_effect(records: &[(String, qk_history::TaskReport)]) -> Option<String> {
    let (mut warm, mut cold) = (Vec::new(), Vec::new());
    for (_, record) in records {
        let Some(state) = &record.warm else {
            continue;
        };
        if record.status != "success"
            || !matches!(record.cache.as_deref(), Some("miss" | "uncached"))
        {
            continue;
        }
        let restored = state.get("restored").is_some_and(|value| !value.is_null());
        let present = state
            .get("present")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|groups| !groups.is_empty());
        if restored || present {
            warm.push(record.duration());
        } else {
            cold.push(record.duration());
        }
    }
    if warm.is_empty() || cold.is_empty() {
        return None;
    }
    let average = |durations: &[u64]| durations.iter().sum::<u64>() / durations.len() as u64;
    let runs = |count: usize| if count == 1 { "run" } else { "runs" };
    Some(format!(
        "From warm state it took {} on average over {} {}; without, {} over {} {}.",
        seconds(average(&warm)),
        warm.len(),
        runs(warm.len()),
        seconds(average(&cold)),
        cold.len(),
        runs(cold.len())
    ))
}

/// An amount of memory, in megabytes below a gigabyte.
pub fn bytes(bytes: u64) -> String {
    if bytes < 1_000_000_000 {
        format!("{} MB", bytes.div_ceil(1_000_000))
    } else {
        format!("{:.1} GB", bytes as f64 / 1e9)
    }
}

/// What warm state did for a run, in words.
pub fn warm_line(warm: &serde_json::Value) -> Option<String> {
    let restored = warm.get("restored").filter(|value| !value.is_null());
    let saved = warm.get("saveMs").and_then(serde_json::Value::as_u64);
    let mut parts = Vec::new();
    if let Some(restored) = restored {
        let number = |field| restored.get(field).and_then(serde_json::Value::as_u64);
        let files = number("files").unwrap_or(0);
        parts.push(format!(
            "warm state restored from {}: {files} file{}, {:.1} MB",
            restored
                .get("source")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?"),
            if files == 1 { "" } else { "s" },
            number("bytes").unwrap_or(0) as f64 / 1e6
        ));
    }
    if let Some(saved) = saved {
        parts.push(format!("warm state saved in {}", seconds(saved)));
    } else if warm.get("background").and_then(serde_json::Value::as_bool) == Some(true) {
        parts.push("warm state saved in the background".to_owned());
    }
    (!parts.is_empty()).then(|| parts.join("; "))
}

/// One line on why a key changed.
fn summary(cause: &Cause) -> String {
    match cause.changed.as_slice() {
        [only] if only == "unchanged" => String::new(),
        [only] if only == "first" => "first run".into(),
        [only] if only == "unknown" => "changed; previous inputs not kept".into(),
        groups => format!("changed: {}", groups.join(", ")),
    }
}

/// Why a key changed, in full.
fn details(cause: &Cause) -> Vec<String> {
    let since = cause
        .previous
        .as_ref()
        .map(|previous| format!(" since run {}", previous.run))
        .unwrap_or_default();
    let mut lines = match cause.changed.as_slice() {
        [only] if only == "unchanged" => vec![format!(
            "same key as run {}",
            cause
                .previous
                .as_ref()
                .map_or("-", |previous| &previous.run)
        )],
        [only] if only == "first" => vec!["first recorded run of this task".into()],
        [only] if only == "unknown" => vec![format!(
            "key changed{since}; the previous inputs are no longer kept"
        )],
        groups => vec![format!("key changed{since}: {}", groups.join(", "))],
    };
    lines.extend(differences(cause));
    lines
}

/// The fields, files, values and dependencies two keys differ in, a line each.
pub fn differences(cause: &Cause) -> Vec<String> {
    const LISTED: usize = 10;
    let mut lines = Vec::new();
    let mut list = |label: &str, items: &[String]| {
        for item in items.iter().take(LISTED) {
            lines.push(format!("  {label} {item}"));
        }
        if items.len() > LISTED {
            lines.push(format!("  {label} ... {} more", items.len() - LISTED));
        }
    };
    list("field", &cause.definition);
    list("changed", &cause.files.changed);
    list("added", &cause.files.added);
    list("removed", &cause.files.removed);
    list("value", &cause.values);
    list("dependency", &cause.dependencies);
    lines
}

fn command(arguments: &[String]) -> String {
    let mut arguments = arguments.iter();
    let program = arguments
        .next()
        .map(|program| {
            Path::new(program)
                .file_name()
                .map_or(program.clone(), |name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_default();
    std::iter::once(program)
        .chain(arguments.cloned())
        .collect::<Vec<_>>()
        .join(" ")
}

fn seconds(milliseconds: u64) -> String {
    format!("{:.1}s", milliseconds as f64 / 1000.0)
}

fn ago(millis: u64) -> String {
    let now = self::millis(SystemTime::now());
    let seconds = now.saturating_sub(millis) / 1000;
    match seconds {
        0..60 => format!("{seconds}s ago"),
        60..3600 => format!("{} min ago", seconds / 60),
        3600..86400 => format!("{} h ago", seconds / 3600),
        _ => format!("{} days ago", seconds / 86400),
    }
}

/// Lists mixed outcomes and the exact executions whose logs can be inspected.
pub fn show_flaky(
    workspace: &Workspace,
    task: Option<&str>,
    limit: usize,
    json: bool,
    out: &mut impl Write,
) -> Result<()> {
    let groups = open(workspace)?.flaky(task, limit)?;
    if json {
        serde_json::to_writer_pretty(&mut *out, &groups)?;
        writeln!(out)?;
        return Ok(());
    }
    if groups.is_empty() {
        writeln!(
            out,
            "No mixed outcomes for identical declared inputs in retained history."
        )?;
    }
    for group in groups {
        writeln!(
            out,
            "{}  key {}  {} passed, {} failed (identical declared inputs)",
            group.task, group.key, group.successes, group.failures
        )?;
        for execution in group.executions {
            writeln!(
                out,
                "  {}  {}  {}",
                execution.run,
                execution.status,
                if execution.log_available {
                    "log retained"
                } else {
                    "log unavailable"
                }
            )?;
        }
    }
    Ok(())
}

/// Replays one execution in recorded chunk order, preserving stdout and stderr.
pub fn show_log(workspace: &Workspace, run: &str, task: &str) -> Result<()> {
    let Some(log) = open(workspace)?.log(run, task)? else {
        bail!(
            "no retained execution log for {task} in run {run} (cache hit, older run or log evicted)"
        );
    };
    qk_executor::read_capture(log.data.as_slice(), |stderr, bytes| {
        if stderr {
            std::io::stderr().lock().write_all(bytes)
        } else {
            std::io::stdout().lock().write_all(bytes)
        }
    })?;
    if log.truncated {
        eprintln!("qk: execution log truncated at 4 MiB");
    }
    Ok(())
}
