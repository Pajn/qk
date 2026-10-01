//! Run history: what each run did to each task, and why a task's key changed.
//!
//! The database is SQLite with a schema that is part of qk's interface,
//! versioned in `schema_version`. Each run records its tasks with their
//! outcome, cache status, key and timing. A task's cause compares its key's
//! inputs with those of the previous key recorded for the same task, grouped
//! as the design names them: files, env, runtime, dependencies, lockfile,
//! definition, args and tooling. Inputs are stored once per key. The newest
//! runs are kept; older ones, and inputs no kept run cites, are removed.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const SCHEMA_VERSION: u32 = 4;
/// Runs kept in the database.
pub const KEPT_RUNS: usize = 200;

/// A run as recorded and as reported by `--report`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RunReport {
    pub schema_version: u32,
    pub id: String,
    pub command: Vec<String>,
    /// The commit checked out, when in a git repository.
    pub sha: Option<String>,
    /// Milliseconds since the Unix epoch.
    pub started: u64,
    pub ended: u64,
    pub exit_code: i32,
    pub tasks: Vec<TaskReport>,
    pub critical_path: CriticalPath,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TaskReport {
    pub id: String,
    pub project: String,
    pub target: String,
    pub configuration: Option<String>,
    /// `success`, `failure`, `cancelled` or `skipped`.
    pub status: String,
    /// `local-hit`, `remote-hit`, `miss` or `uncached`; absent when skipped.
    pub cache: Option<String>,
    pub key: Option<String>,
    pub started: Option<u64>,
    pub ended: Option<u64>,
    pub dependencies: Vec<String>,
    /// Why the key differs from the task's previous one; filled in on record.
    pub cause: Option<Cause>,
    /// What warm state did: where it came from, how much was restored, and
    /// how long saving it took.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warm: Option<Value>,
    /// The threads the task was given, for targets with `qk:threads`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threads: Option<u64>,
}

impl TaskReport {
    pub fn duration(&self) -> u64 {
        match (self.started, self.ended) {
            (Some(started), Some(ended)) => ended.saturating_sub(started),
            _ => 0,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CriticalPath {
    /// From the first task to the last.
    pub tasks: Vec<String>,
    pub duration: u64,
}

/// How a task's key differs from the previous key recorded for it.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Cause {
    /// The run and key compared against; absent the first time.
    pub previous: Option<Previous>,
    /// `unchanged`, `first`, `unknown` (the previous inputs were not kept), or
    /// the groups that differ.
    pub changed: Vec<String>,
    #[serde(default, skip_serializing_if = "Files::is_empty")]
    pub files: Files,
    /// Named values that differ: `env:NAME`, `runtime:COMMAND`, `lockfile …`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<String>,
    /// Dependency tasks whose fingerprints differ.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Previous {
    pub run: String,
    pub key: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Files {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub changed: Vec<String>,
}

impl Files {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }
}

/// A run's summary, for listing.
#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RunSummary {
    pub id: String,
    pub command: Vec<String>,
    pub started: u64,
    pub ended: u64,
    pub exit_code: i32,
    pub tasks: usize,
    /// Tasks by cache status.
    pub cache: BTreeMap<String, usize>,
}

/// Maximum retained framed log payload; run metadata survives log eviction.
pub const MAX_LOG_BYTES: u64 = 64 * 1024 * 1024;

/// Output borrowed from a finished task when recording its run.
pub struct Execution<'a> {
    pub data: &'a [u8],
    pub truncated: bool,
    pub inputs_unchanged: bool,
}

/// A retained execution's stream-tagged output.
pub struct Log {
    pub data: Vec<u8>,
    pub truncated: bool,
}

/// A task/key with both successful and failed actual executions.
#[derive(Debug, Serialize)]
pub struct Flaky {
    pub task: String,
    pub key: String,
    pub successes: usize,
    pub failures: usize,
    pub executions: Vec<Observation>,
}

/// An actual execution contributing to mixed outcomes for a task key.
#[derive(Debug, Serialize)]
pub struct Observation {
    pub run: String,
    pub status: String,
    pub started: u64,
    pub log_available: bool,
}

pub struct History {
    connection: Connection,
}

impl History {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut connection = Connection::open(path)
            .with_context(|| format!("cannot open run history at {}", path.display()))?;
        connection.busy_timeout(std::time::Duration::from_secs(10))?;
        connection.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL);",
        )?;
        // One process at a time creates or upgrades the schema: linked
        // worktrees share the database.
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let version: Option<u32> = transaction
            .query_row("SELECT version FROM schema_version", [], |row| row.get(0))
            .optional()?;
        match version {
            None => {
                transaction.execute_batch(
                    "CREATE TABLE runs (
                         id TEXT PRIMARY KEY,
                         command TEXT NOT NULL,
                         sha TEXT,
                         started INTEGER NOT NULL,
                         ended INTEGER NOT NULL,
                         exit_code INTEGER NOT NULL,
                         critical_path TEXT NOT NULL
                     );
                     CREATE TABLE tasks (
                         run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
                         task_id TEXT NOT NULL,
                         project TEXT NOT NULL,
                         target TEXT NOT NULL,
                         configuration TEXT,
                         status TEXT NOT NULL,
                         cache TEXT,
                         key TEXT,
                         started INTEGER,
                         ended INTEGER,
                         dependencies TEXT NOT NULL,
                         cause TEXT,
                         warm TEXT,
                         threads INTEGER,
                         PRIMARY KEY (run_id, task_id)
                     );
                     CREATE INDEX tasks_by_task ON tasks (task_id, started);
                     CREATE TABLE inputs (key TEXT PRIMARY KEY, inputs TEXT NOT NULL);",
                )?;
                transaction.execute(
                    "INSERT INTO schema_version (version) VALUES (?1)",
                    [SCHEMA_VERSION],
                )?;
            }
            Some(SCHEMA_VERSION) => {}
            // Version 2 added what warm state did for each task, version 3
            // the threads it was given.
            Some(1) => transaction.execute_batch(
                "ALTER TABLE tasks ADD COLUMN warm TEXT;
                 ALTER TABLE tasks ADD COLUMN threads INTEGER;
                 UPDATE schema_version SET version = 3;",
            )?,
            Some(3) => {}
            Some(2) => transaction.execute_batch(
                "ALTER TABLE tasks ADD COLUMN threads INTEGER;
                 UPDATE schema_version SET version = 3;",
            )?,
            Some(other) => {
                bail!("run history has schema version {other}; this qk reads {SCHEMA_VERSION}")
            }
        }
        transaction.execute_batch(
            "CREATE TABLE IF NOT EXISTS execution_logs (
                 run_id TEXT NOT NULL,
                 task_id TEXT NOT NULL,
                 data BLOB,
                 truncated INTEGER NOT NULL,
                 inputs_unchanged INTEGER NOT NULL,
                 PRIMARY KEY (run_id, task_id),
                 FOREIGN KEY (run_id, task_id) REFERENCES tasks(run_id, task_id) ON DELETE CASCADE
             );
             UPDATE schema_version SET version = 4;",
        )?;
        transaction.commit()?;
        connection.execute_batch("PRAGMA foreign_keys = ON;")?;
        Ok(Self { connection })
    }

    /// Records a run, filling in each task's cause against the previous key
    /// recorded for it, and returns the completed report. `inputs` holds what
    /// each key was computed from.
    pub fn record(
        &mut self,
        run: RunReport,
        inputs: &BTreeMap<String, Value>,
    ) -> Result<RunReport> {
        self.record_with_executions(run, inputs, &BTreeMap::new())
    }

    /// Stores execution observations and bounded logs atomically with their run.
    pub fn record_with_executions(
        &mut self,
        mut run: RunReport,
        inputs: &BTreeMap<String, Value>,
        executions: &BTreeMap<&str, Execution<'_>>,
    ) -> Result<RunReport> {
        run.critical_path = critical_path(&run.tasks);
        for task in &mut run.tasks {
            task.cause = match &task.key {
                Some(key) => Some(self.cause(&task.id, key, inputs.get(key))?),
                None => None,
            };
        }
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "INSERT OR REPLACE INTO runs (id, command, sha, started, ended, exit_code, critical_path)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                run.id,
                serde_json::to_string(&run.command)?,
                run.sha,
                run.started,
                run.ended,
                run.exit_code,
                serde_json::to_string(&run.critical_path)?,
            ],
        )?;
        for task in &run.tasks {
            transaction.execute(
                "INSERT OR REPLACE INTO tasks
                     (run_id, task_id, project, target, configuration, status, cache, key,
                      started, ended, dependencies, cause, warm, threads)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                params![
                    run.id,
                    task.id,
                    task.project,
                    task.target,
                    task.configuration,
                    task.status,
                    task.cache,
                    task.key,
                    task.started,
                    task.ended,
                    serde_json::to_string(&task.dependencies)?,
                    task.cause.as_ref().map(serde_json::to_string).transpose()?,
                    task.warm.as_ref().map(serde_json::to_string).transpose()?,
                    task.threads,
                ],
            )?;
        }
        for task in &run.tasks {
            if let Some(execution) = executions.get(task.id.as_str()) {
                // Enforce the limit at the storage boundary as well as capture.
                if execution.data.len() > 4 * 1024 * 1024 {
                    bail!("execution log exceeds 4 MiB");
                }
                transaction.execute(
                    "INSERT INTO execution_logs (run_id, task_id, data, truncated, inputs_unchanged)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![run.id, task.id, execution.data, execution.truncated, execution.inputs_unchanged],
                )?;
            }
        }
        for (key, value) in inputs {
            transaction.execute(
                "INSERT OR IGNORE INTO inputs (key, inputs) VALUES (?1, ?2)",
                params![key, serde_json::to_string(value)?],
            )?;
        }
        transaction.execute(
            "DELETE FROM runs WHERE id NOT IN (SELECT id FROM runs ORDER BY started DESC LIMIT ?1)",
            [KEPT_RUNS],
        )?;
        transaction.execute(
            "DELETE FROM inputs WHERE key NOT IN (SELECT key FROM tasks WHERE key IS NOT NULL)",
            [],
        )?;
        // Keep newest payloads under the shared byte budget, but retain the
        // observations so eviction cannot erase evidence of mixed outcomes.
        transaction.execute(
            "UPDATE execution_logs SET data = NULL WHERE (run_id, task_id) IN (
                 SELECT run_id, task_id FROM (
                     SELECT e.run_id, e.task_id,
                         SUM(length(e.data)) OVER (
                             ORDER BY r.started DESC, e.run_id DESC, e.task_id
                         ) AS retained_bytes
                     FROM execution_logs e JOIN runs r ON r.id = e.run_id
                     WHERE e.data IS NOT NULL
                 ) WHERE retained_bytes > ?1
             )",
            [MAX_LOG_BYTES],
        )?;
        transaction.commit()?;
        Ok(run)
    }

    fn cause(&self, task: &str, key: &str, inputs: Option<&Value>) -> Result<Cause> {
        let previous: Option<(String, String)> = self
            .connection
            .query_row(
                "SELECT run_id, key FROM tasks WHERE task_id = ?1 AND key IS NOT NULL
                 ORDER BY started DESC LIMIT 1",
                [task],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((run, previous_key)) = previous else {
            return Ok(Cause {
                changed: vec!["first".into()],
                ..Cause::default()
            });
        };
        let mut cause = if previous_key == key {
            Cause {
                changed: vec!["unchanged".into()],
                ..Cause::default()
            }
        } else {
            let before: Option<String> = self
                .connection
                .query_row(
                    "SELECT inputs FROM inputs WHERE key = ?1",
                    [&previous_key],
                    |row| row.get(0),
                )
                .optional()?;
            match (before, inputs) {
                (Some(before), Some(after)) => diff(&serde_json::from_str(&before)?, after),
                _ => Cause {
                    changed: vec!["unknown".into()],
                    ..Cause::default()
                },
            }
        };
        cause.previous = Some(Previous {
            run,
            key: previous_key,
        });
        Ok(cause)
    }

    pub fn runs(&self, limit: usize) -> Result<Vec<RunSummary>> {
        let mut statement = self.connection.prepare(
            "SELECT id, command, started, ended, exit_code FROM runs ORDER BY started DESC LIMIT ?1",
        )?;
        let rows = statement
            .query_map([limit], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, u64>(2)?,
                    row.get::<_, u64>(3)?,
                    row.get::<_, i32>(4)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut summaries = Vec::new();
        for (id, command, started, ended, exit_code) in rows {
            let mut cache = BTreeMap::new();
            let mut tasks = 0;
            let mut counts = self.connection.prepare(
                "SELECT COALESCE(cache, status), COUNT(*) FROM tasks WHERE run_id = ?1 GROUP BY 1",
            )?;
            for row in counts.query_map([&id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, usize>(1)?))
            })? {
                let (status, count) = row?;
                tasks += count;
                cache.insert(status, count);
            }
            summaries.push(RunSummary {
                id,
                command: serde_json::from_str(&command)?,
                started,
                ended,
                exit_code,
                tasks,
                cache,
            });
        }
        Ok(summaries)
    }

    /// A recorded run, the latest when `id` is `None`.
    pub fn run(&self, id: Option<&str>) -> Result<Option<RunReport>> {
        let row = match id {
            Some(id) => self
                .connection
                .query_row(
                    "SELECT id, command, sha, started, ended, exit_code, critical_path FROM runs WHERE id = ?1",
                    [id],
                    run_row,
                )
                .optional()?,
            None => self
                .connection
                .query_row(
                    "SELECT id, command, sha, started, ended, exit_code, critical_path FROM runs
                     ORDER BY started DESC LIMIT 1",
                    [],
                    run_row,
                )
                .optional()?,
        };
        let Some((id, command, sha, started, ended, exit_code, critical_path)) = row else {
            return Ok(None);
        };
        let tasks = self.tasks("run_id = ?1", &id, usize::MAX)?;
        Ok(Some(RunReport {
            schema_version: SCHEMA_VERSION,
            id,
            command: serde_json::from_str(&command)?,
            sha,
            started,
            ended,
            exit_code,
            tasks: tasks.into_iter().map(|(_, task)| task).collect(),
            critical_path: serde_json::from_str(&critical_path)?,
        }))
    }

    /// The task's most recent records, newest first, with their runs.
    pub fn task(&self, task: &str, limit: usize) -> Result<Vec<(String, TaskReport)>> {
        self.tasks("task_id = ?1", task, limit)
    }

    /// Reads one execution's log; absent for old runs, cache hits or evicted logs.
    pub fn log(&self, run: &str, task: &str) -> Result<Option<Log>> {
        Ok(self.connection.query_row(
            "SELECT data, truncated FROM execution_logs WHERE run_id = ?1 AND task_id = ?2 AND data IS NOT NULL",
            params![run, task], |row| Ok(Log { data: row.get(0)?, truncated: row.get(1)? })
        ).optional()?)
    }

    /// Mixed outcomes for identical verified declared inputs, excluding cache replay.
    pub fn flaky(&self, task: Option<&str>, limit: usize) -> Result<Vec<Flaky>> {
        let mut statement = self.connection.prepare(
            "SELECT t.task_id, t.key FROM tasks t
             JOIN execution_logs e ON e.run_id = t.run_id AND e.task_id = t.task_id
             WHERE t.cache = 'miss' AND t.key IS NOT NULL AND e.inputs_unchanged = 1
               AND t.status IN ('success', 'failure') AND (?1 IS NULL OR t.task_id = ?1)
             GROUP BY t.task_id, t.key
             HAVING SUM(t.status = 'success') > 0 AND SUM(t.status = 'failure') > 0
             ORDER BY MAX(t.started) DESC, t.task_id, t.key LIMIT ?2",
        )?;
        let keys = statement
            .query_map(
                params![task, i64::try_from(limit).unwrap_or(i64::MAX)],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        keys.into_iter()
            .map(|(task, key)| {
                let mut statement = self.connection.prepare(
                    "SELECT t.run_id, t.status, t.started, e.data IS NOT NULL FROM tasks t
                 JOIN execution_logs e ON e.run_id = t.run_id AND e.task_id = t.task_id
                 WHERE t.task_id = ?1 AND t.key = ?2 AND t.cache = 'miss'
                   AND t.status IN ('success', 'failure') AND e.inputs_unchanged = 1
                 ORDER BY t.started DESC, t.run_id DESC",
                )?;
                let executions = statement
                    .query_map(params![task, key], |row| {
                        Ok(Observation {
                            run: row.get(0)?,
                            status: row.get(1)?,
                            started: row.get(2)?,
                            log_available: row.get(3)?,
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(Flaky {
                    task,
                    key,
                    successes: executions.iter().filter(|e| e.status == "success").count(),
                    failures: executions.iter().filter(|e| e.status == "failure").count(),
                    executions,
                })
            })
            .collect()
    }

    fn tasks(&self, filter: &str, value: &str, limit: usize) -> Result<Vec<(String, TaskReport)>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut statement = self.connection.prepare(&format!(
            "SELECT run_id, task_id, project, target, configuration, status, cache, key,
                    started, ended, dependencies, cause, warm, threads
             FROM tasks WHERE {filter}
             ORDER BY started IS NULL, started DESC, task_id LIMIT ?2"
        ))?;
        let rows = statement
            .query_map(params![value, limit], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, Option<u64>>(8)?,
                    row.get::<_, Option<u64>>(9)?,
                    row.get::<_, String>(10)?,
                    row.get::<_, Option<String>>(11)?,
                    row.get::<_, Option<String>>(12)?,
                    row.get::<_, Option<u64>>(13)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(
                |(
                    run,
                    id,
                    project,
                    target,
                    configuration,
                    status,
                    cache,
                    key,
                    started,
                    ended,
                    dependencies,
                    cause,
                    warm,
                    threads,
                )| {
                    Ok((
                        run,
                        TaskReport {
                            id,
                            project,
                            target,
                            configuration,
                            status,
                            cache,
                            key,
                            started,
                            ended,
                            dependencies: serde_json::from_str(&dependencies)?,
                            cause: cause
                                .map(|cause| serde_json::from_str(&cause))
                                .transpose()?,
                            warm: warm.map(|warm| serde_json::from_str(&warm)).transpose()?,
                            threads,
                        },
                    ))
                },
            )
            .collect()
    }
}

type RunRow = (String, String, Option<String>, u64, u64, i32, String);

fn run_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RunRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
    ))
}

/// The chain of dependent tasks with the longest total duration.
pub fn critical_path(tasks: &[TaskReport]) -> CriticalPath {
    let by_id: BTreeMap<&str, &TaskReport> =
        tasks.iter().map(|task| (task.id.as_str(), task)).collect();
    fn longest<'a>(
        id: &'a str,
        by_id: &BTreeMap<&'a str, &'a TaskReport>,
        memo: &mut BTreeMap<&'a str, (u64, Vec<&'a str>)>,
    ) -> (u64, Vec<&'a str>) {
        if let Some(known) = memo.get(id) {
            return known.clone();
        }
        let task = by_id[id];
        let mut best = (0, Vec::new());
        for dependency in &task.dependencies {
            if by_id.contains_key(dependency.as_str()) {
                let candidate = longest(dependency, by_id, memo);
                if candidate.0 > best.0 {
                    best = candidate;
                }
            }
        }
        best.0 += task.duration();
        best.1.push(id);
        memo.insert(id, best.clone());
        best
    }
    let mut memo = BTreeMap::new();
    let mut best: (u64, Vec<&str>) = (0, Vec::new());
    for id in by_id.keys() {
        let candidate = longest(id, &by_id, &mut memo);
        if candidate.0 > best.0 {
            best = candidate;
        }
    }
    CriticalPath {
        tasks: best.1.into_iter().map(str::to_owned).collect(),
        duration: best.0,
    }
}

/// Which groups of two keys' inputs differ, with the files, values and
/// dependencies that do.
pub fn diff(before: &Value, after: &Value) -> Cause {
    let mut changed = BTreeSet::new();
    let object = |value: &Value, key: &str| {
        value
            .get(key)
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default()
    };
    let mut files = Files::default();
    let (old, new) = (object(before, "files"), object(after, "files"));
    for (path, value) in &new {
        match old.get(path) {
            None => files.added.push(path.clone()),
            Some(previous) if previous != value => files.changed.push(path.clone()),
            Some(_) => {}
        }
    }
    files.removed = old
        .keys()
        .filter(|path| !new.contains_key(*path))
        .cloned()
        .collect();
    if !files.is_empty() {
        changed.insert("files".to_owned());
    }
    let mut values = Vec::new();
    let (old, new) = (object(before, "values"), object(after, "values"));
    for name in old.keys().chain(new.keys()).collect::<BTreeSet<_>>() {
        let (was, is) = (old.get(name), new.get(name));
        if was == is {
            continue;
        }
        let group = if name.starts_with("env:") {
            "env"
        } else if name.starts_with("runtime:") {
            "runtime"
        } else if name == "lockfile" {
            "lockfile"
        } else {
            "inputs"
        };
        changed.insert(group.to_owned());
        if name == "lockfile" {
            values.extend(lockfile_changes(was, is));
        } else {
            values.push(name.clone());
        }
    }
    let (old, new) = (
        object(before, "dependencies"),
        object(after, "dependencies"),
    );
    let dependencies: Vec<String> = old
        .keys()
        .chain(new.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|id| old.get(*id) != new.get(*id))
        .cloned()
        .collect();
    if !dependencies.is_empty() {
        changed.insert("dependencies".to_owned());
    }
    for (group, keys) in [
        ("definition", &["definition", "id"][..]),
        ("args", &["args"][..]),
        (
            "tooling",
            &["schema", "qk", "platform", "packageManager", "workspace"][..],
        ),
    ] {
        if keys.iter().any(|key| before.get(key) != after.get(key)) {
            changed.insert(group.to_owned());
        }
    }
    Cause {
        previous: None,
        changed: changed.into_iter().collect(),
        files,
        values,
        dependencies,
    }
}

/// The parts of the lockfile value that differ: the global part, importers
/// and external packages, by name.
fn lockfile_changes(before: Option<&Value>, after: Option<&Value>) -> Vec<String> {
    let empty = Value::Null;
    let (before, after) = (before.unwrap_or(&empty), after.unwrap_or(&empty));
    let mut changes = Vec::new();
    if before.get("global") != after.get("global") {
        changes.push("lockfile: version, settings or package manager".to_owned());
    }
    for (part, label) in [("importers", "importer"), ("external", "package")] {
        let names = |value: &Value| {
            value
                .get(part)
                .and_then(Value::as_object)
                .map(|object| object.keys().cloned().collect::<BTreeSet<_>>())
                .unwrap_or_default()
        };
        for name in names(before).union(&names(after)) {
            if before.get(part).and_then(|value| value.get(name))
                != after.get(part).and_then(|value| value.get(name))
            {
                changes.push(format!("lockfile: {label} {name}"));
            }
        }
    }
    if changes.is_empty() {
        changes.push("lockfile".to_owned());
    }
    changes
}
