//! What tasks are expected to take, shared through the remote store, so that
//! a machine without local history, such as a CI job on a fresh clone, can
//! order tasks and size their shares of the cores.
//!
//! The store keeps one record per branch, `expectations/<name>`, holding for
//! each task the last few runs that executed it: their time, the cores they
//! held and the memory they used. A run reads the record of its branch, else
//! that of the default branch, as warm state is read. A run that executed
//! tasks adds their runs to the record the store holds at the time, the
//! branch's or else the default branch's, so tasks it did not run keep
//! theirs, and saves it for its branch; the newest save replaces the last.
//! A task is expected to take what [`expect`] makes of its runs, as local
//! history makes of its own. `QK_REMOTE_EXPECTATIONS` narrows what a run may
//! do with them: `read` reads them without adding to them, and `off` leaves
//! them alone.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{SystemTime, UNIX_EPOCH};

use qk_config::Workspace;
use serde::{Deserialize, Serialize};

use crate::{paths, remote, warm};

/// One run of a task, or what a task is expected to take.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Expectation {
    pub millis: u64,
    /// The cores it held: one, unless its target has `qk:threads`.
    pub cores: u64,
    /// The most memory, in bytes, it used, when that was measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<u64>,
}

/// The runs kept of each task, as local history draws on its last five.
const RUNS: usize = 5;
/// How long a task no run has executed keeps its runs in the record.
const KEPT_FOR: u64 = 30 * 24 * 60 * 60 * 1000;
/// The most tasks a record keeps, those executed last.
const TASKS: usize = 10_000;
/// The most bytes a record takes, within the 4 MiB a store server accepts
/// for one, whatever its tasks' ids: the tasks executed longest ago go first.
const BYTES: usize = 4 * 1024 * 1024 - 64 * 1024;

#[derive(Serialize, Deserialize)]
struct Record {
    version: u32,
    /// The name it is stored under, which a store server checks.
    name: String,
    tasks: BTreeMap<String, Runs>,
}

#[derive(Serialize, Deserialize)]
struct Runs {
    /// When a run last added to it, in milliseconds since the epoch.
    updated: u64,
    /// Newest first.
    runs: Vec<Expectation>,
}

/// What a task is expected to take from its recent `runs`: the median of
/// their times, with the cores that run held, and the most memory any of
/// them used, since expecting too little memory is what costs.
pub fn expect(runs: &[Expectation]) -> Option<Expectation> {
    let memory = runs.iter().filter_map(|run| run.memory).max();
    let mut sorted = runs.to_vec();
    sorted.sort_by_key(|run| run.millis);
    let median = *sorted.get(sorted.len() / 2)?;
    Some(Expectation { memory, ..median })
}

/// Whether `bytes` are a record saved for `branch`.
pub(crate) fn usable(bytes: &[u8], branch: &str) -> bool {
    parse(bytes, branch).is_some()
}

/// What each task in the record is expected to take; empty for a record that
/// cannot be read or is not for `branch`. A run that held no core is no
/// measure of its work, so a task expected on one is left out.
pub(crate) fn read(bytes: &[u8], branch: &str) -> BTreeMap<String, Expectation> {
    let Some(record) = parse(bytes, branch) else {
        return BTreeMap::new();
    };
    record
        .tasks
        .into_iter()
        .filter_map(|(id, runs)| {
            let expected = expect(&runs.runs)?;
            (expected.cores > 0).then_some((id, expected))
        })
        .collect()
}

fn parse(bytes: &[u8], branch: &str) -> Option<Record> {
    let record: Record = serde_json::from_slice(bytes).ok()?;
    (record.version == 1 && record.name == remote::expectations_name(branch)).then_some(record)
}

/// The record to save for `branch`: `base`, the record the store holds, with
/// each task's run in `runs` added as its newest at `now`, and the tasks no
/// run has added to for a while left out.
pub(crate) fn merge(
    base: Option<(&[u8], &str)>,
    branch: &str,
    runs: &BTreeMap<String, Expectation>,
    now: u64,
) -> Vec<u8> {
    let mut tasks = base
        .and_then(|(bytes, saved_for)| parse(bytes, saved_for))
        .map(|record| record.tasks)
        .unwrap_or_default();
    for (id, run) in runs {
        let kept = tasks.entry(id.clone()).or_insert(Runs {
            updated: now,
            runs: Vec::new(),
        });
        kept.updated = now;
        kept.runs.insert(0, *run);
        kept.runs.truncate(RUNS);
    }
    tasks.retain(|_, kept| kept.updated.saturating_add(KEPT_FOR) >= now);
    // Newest first, so the tasks dropped are those executed longest ago.
    let mut order: Vec<(u64, String)> = tasks
        .iter()
        .map(|(id, kept)| (kept.updated, id.clone()))
        .collect();
    order.sort_unstable_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    for (_, id) in order.drain(TASKS.min(order.len())..) {
        tasks.remove(&id);
    }
    let mut record = Record {
        version: 1,
        name: remote::expectations_name(branch),
        tasks,
    };
    loop {
        let bytes = serde_json::to_vec(&record).expect("expectations serialize");
        if bytes.len() <= BYTES || record.tasks.is_empty() {
            return bytes;
        }
        // A tenth at a time, so a record far over takes few passes.
        let drop = order.len().div_ceil(10);
        for (_, id) in order.drain(order.len() - drop..) {
            record.tasks.remove(&id);
        }
    }
}

/// What `QK_REMOTE_EXPECTATIONS` lets a run do with the expectations the
/// remote store holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sharing {
    /// Read them, and add to them as the store's mode allows: the default.
    ReadWrite,
    /// Read them only.
    Read,
    /// Neither.
    Off,
}

/// The variable's setting, or why it cannot be used.
fn sharing(environment: &BTreeMap<OsString, OsString>) -> Result<Sharing, String> {
    let value = environment
        .get(std::ffi::OsStr::new("QK_REMOTE_EXPECTATIONS"))
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty());
    match value {
        None | Some("read-write") => Ok(Sharing::ReadWrite),
        Some("read" | "read-only") => Ok(Sharing::Read),
        Some("off") => Ok(Sharing::Off),
        Some(other) => Err(format!(
            "unknown QK_REMOTE_EXPECTATIONS {other:?}; use read-write, read or off"
        )),
    }
}

/// The remote store the workspace configures, when there is one it may use;
/// a store that cannot be configured is left out, as the cache reports it.
fn remote(
    workspace: &Workspace,
    environment: &BTreeMap<OsString, OsString>,
) -> Option<Arc<remote::Remote>> {
    let (s3, server) = crate::remote_settings(workspace);
    remote::configure(s3, server, environment)
        .ok()
        .flatten()
        .map(Arc::new)
}

/// A fetch of the expectations the remote store holds, running beside
/// whatever the run does until it needs them. Dropped unwaited, as when local
/// history has every task, it is left to end on its own, and says nothing:
/// the run does not wait for a fetch whose answer it does not need.
pub struct Fetching(Option<(JoinHandle<Expected>, Arc<AtomicBool>)>);

/// What each task is expected to take, by its id.
type Expected = BTreeMap<String, Expectation>;

impl Fetching {
    /// What each task is expected to take, once the fetch has ended.
    pub fn wait(mut self) -> BTreeMap<String, Expectation> {
        self.0
            .take()
            .and_then(|(fetch, _)| fetch.join().ok())
            .unwrap_or_default()
    }
}

impl Drop for Fetching {
    fn drop(&mut self) {
        if let Some((_, wanted)) = self.0.take() {
            wanted.store(false, Ordering::Relaxed);
        }
    }
}

/// Starts fetching what each task is expected to take from the record the
/// remote store holds for the current branch, else for the default branch:
/// nothing when no store is configured, `QK_REMOTE_EXPECTATIONS` is `off` or
/// the store holds neither. A store that cannot be read is reported.
pub fn fetch(workspace: &Workspace, environment: &BTreeMap<OsString, OsString>) -> Fetching {
    match sharing(environment) {
        Ok(Sharing::Off) => return Fetching(None),
        Ok(_) => {}
        Err(reason) => {
            qk_executor::status!("qk: remote task expectations disabled: {reason}");
            return Fetching(None);
        }
    }
    let Some(remote) = remote(workspace, environment) else {
        return Fetching(None);
    };
    let root = paths::cache_location(workspace);
    let branches = warm::branches(workspace);
    let wanted = Arc::new(AtomicBool::new(true));
    let fetch = std::thread::spawn({
        let wanted = wanted.clone();
        move || match remote.fetch_usable_expectations(&root, &branches) {
            Ok(Some((bytes, branch))) => read(&bytes, &branch),
            Ok(None) => BTreeMap::new(),
            Err(error) => {
                if wanted.load(Ordering::Relaxed) {
                    qk_executor::status!("qk: remote task expectations unavailable ({error:#})");
                }
                BTreeMap::new()
            }
        }
    });
    Fetching(Some((fetch, wanted)))
}

/// Adds each task's run in `runs` to the record the remote store holds, and
/// saves it for the current branch, when the store's mode and
/// `QK_REMOTE_EXPECTATIONS` let it be written and there is a branch to name.
/// Failures are reported without failing the run.
pub fn share(
    workspace: &Workspace,
    environment: &BTreeMap<OsString, OsString>,
    runs: BTreeMap<String, Expectation>,
) {
    if runs.is_empty() || sharing(environment) != Ok(Sharing::ReadWrite) {
        return;
    }
    let Some(remote) =
        remote(workspace, environment).filter(|remote| remote.mode == remote::Mode::ReadWrite)
    else {
        return;
    };
    // A branch's save goes to its own name; without a branch there is none.
    let branches = warm::branches(workspace);
    if warm::current_branch(workspace).is_none() {
        return;
    }
    let root = paths::cache_location(workspace);
    if let Err(error) = std::fs::create_dir_all(root.join("tmp")) {
        qk_executor::status!("qk: remote cache upload failed for task expectations: {error}");
        return;
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64);
    remote.upload_expectations(&root, branches, runs, now);
    for failure in remote.finish() {
        qk_executor::status!("qk: remote cache upload failed for {failure}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(millis: u64, memory: Option<u64>) -> Expectation {
        Expectation {
            millis,
            cores: 2,
            memory,
        }
    }

    #[test]
    fn a_task_expected_on_no_core_is_left_out() {
        let run = |cores| Expectation {
            millis: 1_000,
            cores,
            memory: None,
        };
        let runs: BTreeMap<String, Expectation> = [
            ("app:none".to_owned(), run(0)),
            ("app:one".to_owned(), run(1)),
        ]
        .into();
        let record = merge(None, "main", &runs, 1);
        let expected = read(&record, "main");
        assert!(!expected.contains_key("app:none"));
        assert_eq!(expected["app:one"].cores, 1);
    }

    #[test]
    fn a_record_stays_within_what_a_store_server_accepts() {
        let run = Expectation {
            millis: 1_000,
            cores: 1,
            memory: Some(1 << 30),
        };
        let long = "x".repeat(600);
        let runs: BTreeMap<String, Expectation> = (0..TASKS)
            .map(|index| (format!("project-{index:05}-{long}:test"), run))
            .collect();
        let first = merge(None, "main", &runs, 1);
        assert!(first.len() <= BYTES, "{}", first.len());
        // The tasks run last are the ones kept.
        let newest: BTreeMap<String, Expectation> = [("newest:test".to_owned(), run)].into();
        let second = merge(Some((&first, "main")), "main", &newest, 2);
        assert!(second.len() <= BYTES);
        let record = parse(&second, "main").unwrap();
        assert!(record.tasks.contains_key("newest:test"));
        assert!(record.tasks.len() < TASKS);
    }

    #[test]
    fn the_variable_narrows_what_a_run_does_with_them() {
        let with = |value: &str| {
            let environment = [("QK_REMOTE_EXPECTATIONS".into(), value.into())]
                .into_iter()
                .collect();
            sharing(&environment)
        };
        assert_eq!(sharing(&BTreeMap::new()), Ok(Sharing::ReadWrite));
        assert_eq!(with(""), Ok(Sharing::ReadWrite));
        assert_eq!(with("read-write"), Ok(Sharing::ReadWrite));
        assert_eq!(with("read"), Ok(Sharing::Read));
        assert_eq!(with("read-only"), Ok(Sharing::Read));
        assert_eq!(with("off"), Ok(Sharing::Off));
        assert!(with("no").is_err());
    }

    #[test]
    fn expects_the_median_time_and_the_most_memory() {
        let runs = [run(30, None), run(10, Some(5)), run(20, Some(9))];
        assert_eq!(
            expect(&runs),
            Some(Expectation {
                millis: 20,
                cores: 2,
                memory: Some(9)
            })
        );
        assert_eq!(expect(&[]), None);
    }

    #[test]
    fn a_merge_keeps_the_tasks_a_run_did_not_run() {
        let first = merge(
            None,
            "main",
            &[
                ("app:a".to_owned(), run(10, None)),
                ("app:b".to_owned(), run(5, None)),
            ]
            .into_iter()
            .collect(),
            1_000,
        );
        // Saved for another branch from the default branch's record.
        let second = merge(
            Some((&first, "main")),
            "feature",
            &[("app:a".to_owned(), run(40, None))].into_iter().collect(),
            2_000,
        );
        let read = read(&second, "feature");
        assert_eq!(read["app:a"].millis, 40);
        assert_eq!(read["app:b"].millis, 5);
        // A record is read only under the name it was saved for.
        assert!(super::read(&second, "main").is_empty());
        // Runs are kept, the newest first, up to five.
        let mut record = second;
        for millis in [1, 2, 3, 4, 5] {
            record = merge(
                Some((&record, "feature")),
                "feature",
                &[("app:a".to_owned(), run(millis, None))]
                    .into_iter()
                    .collect(),
                3_000,
            );
        }
        assert_eq!(super::read(&record, "feature")["app:a"].millis, 3);
        // A task no run has executed for a month is dropped.
        let later = merge(
            Some((&record, "feature")),
            "feature",
            &[("app:a".to_owned(), run(1, None))].into_iter().collect(),
            3_000 + KEPT_FOR + 1,
        );
        let kept = super::read(&later, "feature");
        assert!(kept.contains_key("app:a") && !kept.contains_key("app:b"));
    }
}
