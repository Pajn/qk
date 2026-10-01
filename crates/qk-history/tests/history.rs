use std::collections::BTreeMap;

use qk_history::{CriticalPath, History, RunReport, TaskReport, critical_path};
use serde_json::{Value, json};

fn task(
    id: &str,
    key: Option<&str>,
    started: u64,
    ended: u64,
    dependencies: &[&str],
) -> TaskReport {
    let (project, target) = id.split_once(':').unwrap();
    TaskReport {
        id: id.into(),
        project: project.into(),
        target: target.into(),
        configuration: None,
        status: "success".into(),
        cache: Some(if key.is_some() { "miss" } else { "uncached" }.into()),
        key: key.map(str::to_owned),
        started: Some(started),
        ended: Some(ended),
        dependencies: dependencies.iter().map(|id| (*id).to_owned()).collect(),
        cause: None,
        warm: None,
        threads: None,
    }
}

fn run(id: &str, started: u64, tasks: Vec<TaskReport>) -> RunReport {
    RunReport {
        schema_version: qk_history::SCHEMA_VERSION,
        id: id.into(),
        command: vec!["qk".into(), "run-many".into()],
        sha: None,
        started,
        ended: started + 100,
        exit_code: 0,
        tasks,
        critical_path: CriticalPath::default(),
    }
}

fn inputs(entries: &[(&str, Value)]) -> BTreeMap<String, Value> {
    entries
        .iter()
        .map(|(key, value)| ((*key).to_owned(), value.clone()))
        .collect()
}

#[test]
fn explains_what_changed_since_the_previous_key() {
    let temp = tempfile::TempDir::new().unwrap();
    let mut history = History::open(&temp.path().join("history.db")).unwrap();
    let first = history
        .record(
            run("one", 1_000, vec![task("app:build", Some("k1"), 1_000, 1_050, &[])]),
            &inputs(&[(
                "k1",
                json!({"files": {"src/a.ts": "1", "src/gone.ts": "1"}, "values": {"env:MODE": "dev"},
                       "dependencies": {"lib:build": "x"}, "definition": {"command": "tsc"},
                       "lockfile": null}),
            )]),
        )
        .unwrap();
    assert_eq!(first.tasks[0].cause.as_ref().unwrap().changed, ["first"]);

    let second = history
        .record(
            run("two", 2_000, vec![task("app:build", Some("k2"), 2_000, 2_050, &[])]),
            &inputs(&[(
                "k2",
                json!({"files": {"src/a.ts": "2", "src/new.ts": "1"}, "values": {"env:MODE": "prod",
                       "lockfile": {"global": "g", "importers": {".": "a", "apps/app": "b"}, "external": {}}},
                       "dependencies": {"lib:build": "y"}, "definition": {"command": "tsc"}}),
            )]),
        )
        .unwrap();
    let cause = second.tasks[0].cause.clone().unwrap();
    assert_eq!(cause.previous.unwrap().run, "one");
    assert_eq!(cause.changed, ["dependencies", "env", "files", "lockfile"]);
    assert_eq!(cause.files.added, ["src/new.ts"]);
    assert_eq!(cause.files.removed, ["src/gone.ts"]);
    assert_eq!(cause.files.changed, ["src/a.ts"]);
    assert_eq!(cause.dependencies, ["lib:build"]);
    assert!(cause.values.contains(&"env:MODE".to_owned()));
    assert!(
        cause
            .values
            .contains(&"lockfile: importer apps/app".to_owned()),
        "{:?}",
        cause.values
    );

    let third = history
        .record(
            run(
                "three",
                3_000,
                vec![task("app:build", Some("k2"), 3_000, 3_001, &[])],
            ),
            &BTreeMap::new(),
        )
        .unwrap();
    assert_eq!(
        third.tasks[0].cause.as_ref().unwrap().changed,
        ["unchanged"]
    );

    // Stored and read back as recorded.
    assert_eq!(history.run(None).unwrap().unwrap(), third);
    assert_eq!(history.run(Some("two")).unwrap().unwrap(), second);
    let records = history.task("app:build", 10).unwrap();
    assert_eq!(
        records
            .iter()
            .map(|(run, _)| run.as_str())
            .collect::<Vec<_>>(),
        ["three", "two", "one"]
    );
    let runs = history.runs(10).unwrap();
    assert_eq!(runs.len(), 3);
    assert_eq!(runs[0].cache["miss"], 1);
}

#[test]
fn critical_path_follows_the_longest_dependency_chain() {
    let tasks = vec![
        task("lib:build", None, 0, 50, &[]),
        task("gen:build", None, 0, 10, &[]),
        task("app:build", None, 50, 80, &["lib:build", "gen:build"]),
        task("docs:build", None, 0, 60, &[]),
    ];
    let path = critical_path(&tasks);
    assert_eq!(path.tasks, ["lib:build", "app:build"]);
    assert_eq!(path.duration, 80);
}

#[test]
fn upgrades_a_history_from_before_warm_state() {
    let temp = tempfile::TempDir::new().unwrap();
    let path = temp.path().join("history.db");
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch(
            "CREATE TABLE schema_version (version INTEGER NOT NULL);
             INSERT INTO schema_version (version) VALUES (1);
             CREATE TABLE runs (
                 id TEXT PRIMARY KEY, command TEXT NOT NULL, sha TEXT,
                 started INTEGER NOT NULL, ended INTEGER NOT NULL,
                 exit_code INTEGER NOT NULL, critical_path TEXT NOT NULL
             );
             CREATE TABLE tasks (
                 run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
                 task_id TEXT NOT NULL, project TEXT NOT NULL, target TEXT NOT NULL,
                 configuration TEXT, status TEXT NOT NULL, cache TEXT, key TEXT,
                 started INTEGER, ended INTEGER, dependencies TEXT NOT NULL, cause TEXT,
                 PRIMARY KEY (run_id, task_id)
             );
             CREATE INDEX tasks_by_task ON tasks (task_id, started);
             CREATE TABLE inputs (key TEXT PRIMARY KEY, inputs TEXT NOT NULL);
             INSERT INTO runs VALUES ('old', '[\"qk\"]', NULL, 1000, 1100, 0, '{\"tasks\":[],\"duration\":0}');
             INSERT INTO tasks VALUES ('old', 'app:build', 'app', 'build', NULL, 'success',
                 'miss', 'k0', 1000, 1050, '[]', NULL);",
        )
        .unwrap();
    let mut history = History::open(&path).unwrap();
    let mut warm = task("app:build", Some("k1"), 2_000, 2_050, &[]);
    warm.warm = Some(json!({"restored": {"source": "local", "files": 1}, "saveMs": 3}));
    history
        .record(run("new", 2_000, vec![warm]), &BTreeMap::new())
        .unwrap();
    let tasks = history.task("app:build", 10).unwrap();
    let warm_of = |run: &str| {
        tasks
            .iter()
            .find(|(id, _)| id == run)
            .map(|(_, task)| task.warm.clone())
            .unwrap()
    };
    assert_eq!(warm_of("old"), None);
    assert_eq!(warm_of("new").unwrap()["saveMs"], 3);
    drop(history);
    History::open(&path).unwrap();
}

/// Different keys, replays, cancellations and unstable inputs cannot imply flakiness.
#[test]
fn mixed_outcomes_require_verified_executions_of_the_same_task_key() {
    let temp = tempfile::tempdir().unwrap();
    let mut history = History::open(&temp.path().join("history.db")).unwrap();
    for (index, (id, key, status, cache, stable)) in [
        ("app:test", "same", "failure", "miss", true),
        ("app:test", "different", "success", "miss", true),
        ("other:test", "same", "success", "miss", true),
        ("app:test", "same", "success", "local-hit", true),
        ("app:test", "same", "success", "miss", false),
        ("app:test", "same", "cancelled", "miss", true),
        ("app:test", "same", "success", "miss", true),
    ]
    .into_iter()
    .enumerate()
    {
        let mut record = task(id, Some(key), index as u64, index as u64 + 1, &[]);
        record.status = status.into();
        record.cache = Some(cache.into());
        history
            .record_with_executions(
                run(&index.to_string(), index as u64, vec![record]),
                &BTreeMap::new(),
                &BTreeMap::from([(
                    id,
                    qk_history::Execution {
                        data: b"",
                        truncated: false,
                        inputs_unchanged: stable,
                    },
                )]),
            )
            .unwrap();
        let groups = history.flaky(None, 20).unwrap();
        if index < 6 {
            assert!(groups.is_empty());
        } else {
            assert_eq!(groups.len(), 1);
            assert_eq!(groups[0].successes, 1);
            assert_eq!(groups[0].failures, 1);
        }
    }
    assert!(history.flaky(Some("other:test"), 20).unwrap().is_empty());
    assert!(history.flaky(None, 0).unwrap().is_empty());
}

/// Payload eviction leaves observations intact; run pruning removes their logs.
#[test]
fn log_retention_is_bounded_without_erasing_mixed_outcomes() {
    let temp = tempfile::tempdir().unwrap();
    let mut history = History::open(&temp.path().join("history.db")).unwrap();
    let data = vec![0; 4 * 1024 * 1024];
    for index in 0..18 {
        let mut record = task("app:test", Some("same"), index, index + 1, &[]);
        if index == 0 {
            record.status = "failure".into();
        }
        history
            .record_with_executions(
                run(&index.to_string(), index, vec![record]),
                &BTreeMap::new(),
                &BTreeMap::from([(
                    "app:test",
                    qk_history::Execution {
                        data: &data,
                        truncated: true,
                        inputs_unchanged: true,
                    },
                )]),
            )
            .unwrap();
    }
    assert!(history.log("0", "app:test").unwrap().is_none());
    let newest = history.log("17", "app:test").unwrap().unwrap();
    assert!(newest.truncated);
    assert_eq!(newest.data.len(), data.len());
    let groups = history.flaky(None, 20).unwrap();
    assert_eq!(groups[0].failures, 1);
    assert_eq!(
        groups[0]
            .executions
            .iter()
            .filter(|e| e.log_available)
            .count(),
        16
    );
    for index in 18..220 {
        history
            .record(
                run(
                    &index.to_string(),
                    index,
                    vec![task("app:test", Some("same"), index, index + 1, &[])],
                ),
                &BTreeMap::new(),
            )
            .unwrap();
    }
    assert!(history.log("17", "app:test").unwrap().is_none());
    assert!(history.flaky(None, 20).unwrap().is_empty());
}

/// Existing schema-3 runs remain readable but do not invent execution observations.
#[test]
fn upgrades_schema_three_without_inventing_logs() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("history.db");
    let mut history = History::open(&path).unwrap();
    history
        .record(
            run("old", 1, vec![task("app:test", Some("same"), 1, 2, &[])]),
            &BTreeMap::new(),
        )
        .unwrap();
    drop(history);
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch("DROP TABLE execution_logs; UPDATE schema_version SET version = 3;")
        .unwrap();
    let history = History::open(&path).unwrap();
    assert!(history.run(Some("old")).unwrap().is_some());
    assert!(history.log("old", "app:test").unwrap().is_none());
    assert!(history.flaky(None, 20).unwrap().is_empty());
}

#[test]
fn a_task_is_expected_to_take_the_median_of_its_recent_executions() {
    let temp = tempfile::TempDir::new().unwrap();
    let mut history = History::open(&temp.path().join("history.db")).unwrap();
    // Oldest first: the first two fall out of the last five executions.
    for (index, millis) in [900, 900, 40, 10, 30, 50, 20].into_iter().enumerate() {
        let started = 1_000 * (index as u64 + 1);
        let mut test = task("app:test", Some("k"), started, started + millis, &[]);
        test.threads = Some(4);
        history
            .record(
                run(&format!("run{index}"), started, vec![test]),
                &inputs(&[]),
            )
            .unwrap();
    }
    // Cache hits and cancelled tasks do not say how long the task takes.
    let mut hit = task("app:test", Some("k"), 9_000, 9_001, &[]);
    hit.cache = Some("local-hit".into());
    let mut cancelled = task("app:lint", None, 9_000, 9_002, &[]);
    cancelled.status = "cancelled".into();
    history
        .record(run("hit", 9_000, vec![hit, cancelled]), &inputs(&[]))
        .unwrap();
    let expected = history.expected().unwrap();
    assert_eq!(
        expected.get("app:test"),
        Some(&qk_history::Expected {
            millis: 30,
            threads: 4
        })
    );
    assert_eq!(expected.get("app:lint"), None);
}
