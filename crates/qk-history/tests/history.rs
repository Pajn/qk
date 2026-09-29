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
