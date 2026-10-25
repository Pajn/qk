//! Local content-addressed cache shared by linked Git worktrees.

mod evict;
pub mod expectations;
mod glob;
mod hash;
mod ignored;
mod inputs;
mod paths;
pub mod profile;
mod record;
pub mod recorded;
mod remote;
mod store;
pub mod ultracache;
pub mod warm;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use qk_config::Workspace;
use qk_executor::{Capture, Outcome, PreparedTask, execute, execute_captured};
use qk_taskgraph::{Task, TaskGraph};
use serde_json::Value;

pub use evict::{Pruned, max_size, parse_size, prune, prune_automatic};
pub use glob::Pattern;
pub use hash::{
    DependencyGlob, Selects, SourceIgnore, dependency_globs, keyed_definition, source_files,
    task_configuration,
};
pub use inputs::{
    InputChange, InputChanges, JsonFields, ROOT_TSCONFIG_FILES, Resolution, Resolved, TsConfig,
    manifest_for_dependents, resolve_each, resolve_tasks, selectively_hash_tsconfig,
    without_resolution,
};
/// Expands `{projectRoot}` and `{workspaceRoot}` in a project's path or glob.
pub use paths::expand as expand_project_path;
pub use paths::git as workspace_git;
pub use paths::{
    Outputs, cache_directory, cache_location, clear_worktree_state, resolved_outputs,
    worktree_state,
};

/// The nx.json settings of the workspace's remote store: its `s3`, and its
/// `storeServer` or `qk:storeServer`. Nx never reads `qk:storeServer`, so a
/// workspace can name a store server for qk while Nx's own cache plugin keeps
/// `s3`.
fn remote_settings(workspace: &Workspace) -> (Option<&Value>, Option<&Value>) {
    let extra = &workspace.config.extra;
    match extra.get("qk:storeServer") {
        Some(server) => (None, Some(server)),
        None => (extra.get("s3"), extra.get("storeServer")),
    }
}

/// The cache for one run; its workspace snapshot is taken on first use.
pub struct Cache {
    pub root: PathBuf,
    snapshot: OnceLock<std::result::Result<hash::Snapshot, String>>,
    remote: Option<std::sync::Arc<remote::Remote>>,
    /// Whether results and warm state may be restored or published.
    reuse: bool,
    /// The tasks whose outputs this run left exactly as they were.
    kept: std::sync::Mutex<std::collections::BTreeSet<String>>,
    /// Warm state being saved in the background.
    saves: std::sync::Mutex<Vec<std::thread::JoinHandle<()>>>,
    cleanup: std::sync::Mutex<Option<store::Cleanup>>,
    /// Every target's declared outputs, which recordings' listings leave out.
    workspace_outputs: OnceLock<recorded::WorkspaceOutputs>,
}

/// Where a task's result came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CacheStatus {
    LocalHit,
    RemoteHit,
    Miss,
    /// Ran without consulting the cache: not cacheable, bypassed or disabled.
    Uncached,
}

/// One actual keyed execution, independent of reusable cache entries.
pub struct Execution {
    pub log: qk_executor::RecordedLog,
    pub inputs_unchanged: bool,
}

pub struct TaskResult {
    pub outcome: Outcome,
    pub cache: CacheStatus,
    /// The task's key and what it was computed from, when it has one.
    pub key: Option<(String, Value)>,
    /// What warm state did, for targets that keep it.
    pub warm: Option<warm::WarmReport>,
    pub execution: Option<Execution>,
    /// How a task keyed by its recordings was keyed: see [`recorded`].
    pub recorded: Option<Recorded>,
    /// For a task with `qk:ultracache` mode `"record"`, what its recordings
    /// would have done.
    pub shadow: Option<Shadow>,
}

/// What the recordings of a task keyed by its declared inputs would have done.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum Shadow {
    /// One of its path sets' keys had a result, here or in the remote store.
    WouldHit { key: String },
    /// None of its path sets' keys had a result.
    WouldMiss,
    /// It has no path set yet.
    Unrecorded,
}

/// A lookup by a task's recordings in mode `"record"`, started before its
/// lookup by its declared key so that the two wait on the remote store
/// together rather than one after the other.
enum Shadowing {
    /// Answered without the store: a recording's result is held here, or
    /// there is no store to ask.
    Settled(Shadow),
    /// Waiting for the path sets the store holds, with the keys of the path
    /// sets held here, none of which has a result here.
    Fetching {
        keys: Vec<String>,
        fetch: std::thread::JoinHandle<Result<Option<Vec<u8>>>>,
    },
}

/// The key of one of a task's path sets, or none, said, when the path set
/// cannot key it.
fn recording_key(
    weak: &str,
    set: &recorded::PathSet,
    snapshot: &hash::Snapshot,
    workspace: &Workspace,
    unlisted: &recorded::Unlisted,
    task: &Task,
) -> Option<String> {
    recorded::strong_key(weak, set, snapshot, &workspace.root, unlisted)
        .inspect_err(|error| {
            qk_executor::status!("qk: {}: ignoring a path set ({error:#})", task.id);
        })
        .ok()
}

/// What a lookup by a task's recordings found.
enum Found {
    /// A result under this key in the local cache.
    Local(String),
    /// A result under this key in the remote store.
    Remote(String),
    Nothing,
}

/// How a task with `qk:ultracache` was keyed.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum Recorded {
    /// One of its path sets' keys had a result, restored under that key.
    Hit { key: String },
    /// This run recorded it, and stored its result under its new key.
    Saved { key: String },
    /// None of its path sets' keys had a result, and this run did not record.
    Missed,
    /// It has no path set yet, and this run did not record one: it was keyed
    /// by its declared inputs.
    Unrecorded,
    /// None of its path sets' keys had a result, and its declared key, which
    /// covers what each of them read, did: restored under that key.
    Declared { key: String },
    /// Recording it failed, with why.
    Failed { reason: String },
}

impl TaskResult {
    pub fn uncached(outcome: Outcome) -> Self {
        Self {
            outcome,
            cache: CacheStatus::Uncached,
            key: None,
            warm: None,
            execution: None,
            recorded: None,
            shadow: None,
        }
    }
}

impl Cache {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            snapshot: OnceLock::new(),
            remote: None,
            reuse: true,
            kept: Default::default(),
            saves: Default::default(),
            cleanup: Default::default(),
            workspace_outputs: OnceLock::new(),
        }
    }

    /// Records verified executions without configuring or using result stores.
    pub fn for_observation(workspace: &Workspace) -> Self {
        Self {
            reuse: false,
            ..Self::new(paths::cache_location(workspace))
        }
    }

    /// The workspace's cache, with the remote store its nx.json `s3`,
    /// `storeServer` or `qk:storeServer` key configures, if any. Credentials
    /// and modes are read from `environment`, which includes the workspace's
    /// dotenv files. A remote store that cannot be used is reported and left
    /// out; the local cache still works.
    pub fn for_workspace(
        workspace: &Workspace,
        environment: &BTreeMap<std::ffi::OsString, std::ffi::OsString>,
    ) -> Self {
        let root = paths::cache_location(workspace);
        let (s3, server) = remote_settings(workspace);
        let remote = match remote::configure(s3, server, environment) {
            Ok(remote) => remote.map(std::sync::Arc::new),
            Err(error) => {
                qk_executor::status!("qk: remote cache disabled: {error:#}");
                None
            }
        };
        if let Some(remote) = &remote {
            remote.read_index(&root);
        }
        Self {
            remote,
            ..Self::new(root)
        }
    }

    /// Saves what later runs reuse and waits for background saves, uploads
    /// and retired-output cleanup, reporting failures.
    pub fn finish(&self, workspace: &Workspace) {
        if !self.reuse {
            return;
        }
        if let Some(Ok(snapshot)) = self.snapshot.get() {
            snapshot.save_digests(&workspace.root);
        }
        // Before the uploads, which a save may add to.
        for save in std::mem::take(&mut *self.saves.lock().unwrap()) {
            let _ = save.join();
        }
        if let Some(remote) = &self.remote {
            let failures = remote.finish();
            for failure in &failures {
                qk_executor::status!("qk: remote cache upload failed for {failure}");
            }
        }
        let _profile = profile::span("cache", "restore_cleanup_wait");
        self.cleanup.lock().unwrap().take();
    }

    fn snapshot(
        &self,
        workspace: &Workspace,
        graph: &TaskGraph,
    ) -> std::result::Result<&hash::Snapshot, String> {
        self.snapshot
            .get_or_init(|| {
                hash::Snapshot::new(workspace, graph, &self.root)
                    .map_err(|error| format!("cannot snapshot the workspace: {error:#}"))
            })
            .as_ref()
            .map_err(Clone::clone)
    }

    pub fn run(
        &self,
        workspace: &Workspace,
        graph: &TaskGraph,
        task: &Task,
        prepared: &PreparedTask,
        cancelled: &AtomicBool,
    ) -> Result<TaskResult> {
        let result = self.run_task(workspace, graph, task, prepared, cancelled);
        if let Some(Ok(snapshot)) = self.snapshot.get() {
            snapshot.outputs_written(workspace, task);
        }
        result
    }

    fn run_task(
        &self,
        workspace: &Workspace,
        graph: &TaskGraph,
        task: &Task,
        prepared: &PreparedTask,
        cancelled: &AtomicBool,
    ) -> Result<TaskResult> {
        let cacheable = task.definition.cache == Some(true);
        // Held until the task is done, whether it ran or came from the cache.
        let _kept = self
            .reuse
            .then(|| warm::keep_across(workspace, graph, task));
        let entries = if self.reuse {
            warm::config(workspace, task).unwrap_or_else(|error| {
                qk_executor::status!("qk: {}: warm state ignored ({error:#})", task.id);
                Vec::new()
            })
        } else {
            Vec::new()
        };
        // Names an entry in messages: the group it shares, if any.
        let entry_label = |warm: &warm::Warm| match warm.group() {
            Some(group) => format!(" (group {group})"),
            None => String::new(),
        };
        // The warm variables point tools at their state; they are not inputs,
        // so the key is computed from the task without them.
        let mut running = prepared.clone();
        for warm in &entries {
            for (name, value) in &warm.env {
                running.execution.insert(name.into(), value.into());
            }
        }
        // What each entry restored, and which groups were on disk already.
        let before = |running: &PreparedTask| -> (Vec<warm::Restored>, Vec<String>) {
            let (mut restored, mut present) = (Vec::new(), Vec::new());
            for warm in &entries {
                match self
                    .initialize()
                    .and_then(|()| self.restore_warm(workspace, task, warm, running))
                {
                    Ok((entry, groups)) => {
                        if !entry.groups.is_empty() {
                            restored.push(entry);
                        }
                        present.extend(groups);
                    }
                    Err(error) => qk_executor::status!(
                        "qk: {}: warm state{} not restored ({error:#})",
                        task.id,
                        entry_label(warm)
                    ),
                }
            }
            (restored, present)
        };
        let after = |outcome: Outcome,
                     (restored, present): (Vec<warm::Restored>, Vec<String>),
                     running: &PreparedTask|
         -> Option<warm::WarmReport> {
            if entries.is_empty() {
                return None;
            }
            let mut save_ms = None;
            let mut background = false;
            for warm in entries.iter().filter(|_| outcome == Outcome::Success) {
                let started = std::time::Instant::now();
                let saved = if warm.background {
                    self.save_warm_in_background(workspace, task, warm, running)
                } else {
                    self.save_warm(workspace, task, warm, running)
                };
                match saved {
                    Ok(()) if warm.background => background = true,
                    Ok(()) => {
                        *save_ms.get_or_insert(0) += started.elapsed().as_millis() as u64;
                    }
                    Err(error) => qk_executor::status!(
                        "qk: {}: warm state{} not saved ({error:#})",
                        task.id,
                        entry_label(warm)
                    ),
                }
            }
            Some(warm::WarmReport {
                restored,
                present,
                save_ms,
                background,
            })
        };
        // Reports why this task runs uncached.
        let bypass = |reason: String| {
            if cacheable {
                qk_executor::status!("qk: {}: cache bypassed ({reason})", task.id);
            }
            let _profile = profile::span(&task.id, "execute");
            execute(prepared, cancelled).map(TaskResult::uncached)
        };
        let snapshot = match self.snapshot(workspace, graph) {
            Ok(snapshot) => snapshot,
            Err(reason) => return bypass(reason),
        };
        let dependency_profile = profile::span(&task.id, "dependency_inputs");
        let dependencies = match hash::dependency_keys(snapshot, workspace, graph, task, cancelled)
        {
            Ok(dependencies) => dependencies,
            Err(error) => return bypass(format!("{error:#}")),
        };
        drop(dependency_profile);
        let input_profile = profile::span(&task.id, "inputs");
        let (key, inputs) = match hash::inputs(
            snapshot,
            workspace,
            task,
            prepared,
            &dependencies,
            cancelled,
        )
        .and_then(|inputs| Ok((hash::key(&inputs)?, inputs)))
        {
            Ok(key) => key,
            Err(error) => return bypass(format!("{error:#}")),
        };
        drop(input_profile);
        let keyed = Some((key.clone(), inputs));
        // As in Nx, the task sees its hash.
        running
            .execution
            .insert("NX_TASK_HASH".into(), key.clone().into());
        let running = &running;
        let outputs = match paths::Outputs::new(workspace, task) {
            Ok(outputs) => outputs,
            Err(error) => return bypass(format!("{error:#}")),
        };
        if !cacheable {
            let restored = before(running);
            let executing = profile::span(&task.id, "execute");
            let outcome = execute(running, cancelled)?;
            drop(executing);
            let warm_report = after(outcome, restored, running);
            return Ok(TaskResult {
                outcome,
                cache: CacheStatus::Uncached,
                key: keyed,
                warm: warm_report,
                execution: None,
                recorded: None,
                shadow: None,
            });
        }
        if !self.reuse {
            let capture = Capture::new(None, prepared.display.clone()).retain_log();
            let executing = profile::span(&task.id, "execute");
            let outcome = execute_captured(running, cancelled, Some(&capture))?;
            drop(executing);
            let inputs_unchanged =
                self.inputs_match(snapshot, workspace, graph, task, prepared, &key, cancelled);
            return Ok(TaskResult {
                outcome,
                cache: CacheStatus::Uncached,
                key: keyed,
                warm: None,
                execution: capture.take_log().map(|log| Execution {
                    log,
                    inputs_unchanged,
                }),
                recorded: None,
                shadow: None,
            });
        }
        if let Err(error) = self.initialize() {
            return bypass(format!("cache unavailable: {error}"));
        }
        let (_lock, waited) = match self.lock(&key, cancelled) {
            Ok(Some(lock)) => lock,
            Ok(None) => {
                return Ok(TaskResult::uncached(Outcome::Cancelled));
            }
            Err(error) => return bypass(format!("cache lock unavailable: {error}")),
        };
        // Inputs may have changed while another worktree held this key's lock.
        if waited
            && hash::recheck_dependency_keys(snapshot, workspace, graph, task, cancelled)
                .and_then(|dependencies| {
                    hash::fingerprint(
                        snapshot,
                        workspace,
                        task,
                        prepared,
                        &dependencies,
                        cancelled,
                    )
                })
                .ok()
                .as_ref()
                != Some(&key)
        {
            return bypass("inputs changed while waiting for another run".into());
        }
        // Keyed by its recordings, a task's path sets stand in for its
        // declared file inputs: see `recorded`.
        let settings = ultracache::Settings::of(task).unwrap_or_else(|error| {
            qk_executor::status!("qk: {}: qk:ultracache ignored ({error:#})", task.id);
            None
        });
        let warm_outputs = paths::Outputs::from_paths(&warm::kept_paths(&entries)).ok();
        let own: Vec<&paths::Outputs> = std::iter::once(&outputs)
            .chain(warm_outputs.as_ref())
            .collect();
        // What its recordings' listings leave out, by the workspace's
        // configuration rather than what this run schedules, so that the
        // same recording keys the same in any run.
        let others = settings.as_ref().map(|_| recorded::Others {
            outputs: self
                .workspace_outputs
                .get_or_init(|| workspace_outputs(workspace)),
            upstream: upstream(graph, task),
        });
        let ignore: Vec<Pattern> = settings
            .as_ref()
            .map(|settings| {
                settings
                    .ignore_reads
                    .iter()
                    .filter_map(|glob| Pattern::new(glob, false).ok())
                    .collect()
            })
            .unwrap_or_default();
        let unlisted = recorded::Unlisted {
            own: own.clone(),
            others,
            ignore: &ignore,
        };
        let inputs = &keyed.as_ref().expect("a cacheable task is keyed").1;
        let mut recordings = settings.and_then(|settings| {
            let weak = hash::weak_key(
                snapshot,
                workspace,
                graph,
                task,
                prepared,
                inputs,
                &settings.ignore_reads,
                cancelled,
            );
            match weak {
                Ok(weak) => Some((recorded::load(&self.root, &weak), weak, settings)),
                Err(error) => {
                    qk_executor::status!(
                        "qk: {}: not keyed by its recordings ({error:#})",
                        task.id
                    );
                    None
                }
            }
        });
        let mut shadowing = None;
        if let Some((sets, weak, settings)) = &mut recordings {
            if settings.mode == ultracache::Mode::Record {
                let _lookup = profile::span(&task.id, "recorded_lookup");
                shadowing =
                    Some(self.start_shadow(workspace, task, snapshot, &unlisted, sets, weak));
            } else {
                let recorded_profile = profile::span(&task.id, "recorded_restore");
                let found = self.by_recordings(
                    workspace, task, prepared, snapshot, &outputs, &unlisted, sets, weak, &key,
                    cancelled,
                );
                drop(recorded_profile);
                if let found @ (Found::Local(_) | Found::Remote(_)) = found {
                    let (strong, remote) = match found {
                        Found::Local(strong) => {
                            if let Some(remote) = &self.remote {
                                remote.held(&strong);
                            }
                            (strong, None)
                        }
                        Found::Remote(strong) => (strong, Some(())),
                        Found::Nothing => unreachable!(),
                    };
                    recorded::touch(&self.root, weak);
                    qk_executor::report::event(qk_executor::report::Event::Cache {
                        id: task.id.clone(),
                        cached: if remote.is_some() {
                            qk_executor::report::Cached::RemoteHit
                        } else {
                            qk_executor::report::Cached::Hit
                        },
                    });
                    return Ok(TaskResult {
                        outcome: Outcome::Success,
                        cache: if remote.is_some() {
                            CacheStatus::RemoteHit
                        } else {
                            CacheStatus::LocalHit
                        },
                        key: keyed,
                        warm: None,
                        execution: None,
                        recorded: Some(Recorded::Hit { key: strong }),
                        shadow: None,
                    });
                }
            }
        }
        // What its recordings would have done, once the store has said.
        let mut settle = |recordings: &mut Option<(Vec<recorded::PathSet>, String, _)>| {
            let shadowing = shadowing.take()?;
            let (sets, weak, _) = recordings.as_mut()?;
            let _wait = profile::span(&task.id, "recorded_wait");
            Some(self.settle_shadow(shadowing, workspace, task, snapshot, &unlisted, sets, weak))
        };
        // A task with path sets is keyed by them alone, unless its declared
        // inputs, as they are now, cover what each of them read: otherwise
        // its declared inputs are what a recording found wanting. In mode
        // "record" it is keyed by its declared inputs all the same.
        let declared = recordings.as_ref().is_none_or(|(sets, _, settings)| {
            settings.mode == ultracache::Mode::Record
                || sets.is_empty()
                || hash::coverage(snapshot, workspace, graph, task, prepared, cancelled).is_ok_and(
                    |coverage| {
                        let outputs = self
                            .workspace_outputs
                            .get_or_init(|| workspace_outputs(workspace));
                        let output = |path: &str| outputs.matches(path);
                        sets.iter()
                            .all(|set| coverage.covers(snapshot, &workspace.root, set, &output))
                    },
                )
        });
        // How its recordings keyed it, unless it runs and records.
        let unrecorded = recordings
            .as_ref()
            .filter(|(_, _, settings)| settings.mode == ultracache::Mode::On)
            .map(|(sets, _, _)| {
                if sets.is_empty() {
                    Recorded::Unrecorded
                } else {
                    Recorded::Declared { key: key.clone() }
                }
            });
        // An entry fetched ahead of this task is local, but it came from the
        // remote store, and is reported as such.
        let prefetched = declared
            && self.remote.as_ref().is_some_and(|remote| {
                remote.resolved(&key);
                remote.prefetched(&key)
            });
        let local_profile = profile::span(&task.id, "local_restore");
        let restored = if declared {
            self.restore(
                &workspace.root,
                &task.id,
                &task.dependencies,
                &key,
                &outputs,
                &prepared.display,
                if prefetched {
                    qk_executor::Shown::RemoteCache
                } else {
                    qk_executor::Shown::LocalCache
                },
            )
        } else {
            Ok(false)
        };
        match restored {
            Ok(true) => {
                let claimed = match &self.remote {
                    Some(remote) if prefetched && remote.claim(&key) => true,
                    Some(remote) => {
                        remote.held(&key);
                        false
                    }
                    None => false,
                };
                qk_executor::report::event(qk_executor::report::Event::Cache {
                    id: task.id.clone(),
                    cached: if claimed {
                        qk_executor::report::Cached::RemoteHit
                    } else {
                        qk_executor::report::Cached::Hit
                    },
                });
                return Ok(TaskResult {
                    outcome: Outcome::Success,
                    cache: if claimed {
                        CacheStatus::RemoteHit
                    } else {
                        CacheStatus::LocalHit
                    },
                    key: keyed,
                    warm: None,
                    execution: None,
                    recorded: unrecorded,
                    shadow: settle(&mut recordings),
                });
            }
            Ok(false) => {}
            Err(error) => {
                qk_executor::status!("qk: {}: ignoring unusable cache entry ({error})", task.id)
            }
        }
        drop(local_profile);
        if let Some(remote) = self.remote.as_ref().filter(|_| declared) {
            let fetch_profile = profile::span(&task.id, "remote_fetch");
            let fetched = remote.fetch(&self.root, &key, &task.id, cancelled);
            drop(fetch_profile);
            let fetched = fetched.and_then(|found| {
                if !found {
                    return Ok(false);
                }
                self.restore(
                    &workspace.root,
                    &task.id,
                    &task.dependencies,
                    &key,
                    &outputs,
                    &prepared.display,
                    qk_executor::Shown::RemoteCache,
                )
            });
            match fetched {
                Ok(true) => {
                    qk_executor::report::event(qk_executor::report::Event::Cache {
                        id: task.id.clone(),
                        cached: qk_executor::report::Cached::RemoteHit,
                    });
                    return Ok(TaskResult {
                        outcome: Outcome::Success,
                        cache: CacheStatus::RemoteHit,
                        key: keyed,
                        warm: None,
                        execution: None,
                        recorded: unrecorded,
                        shadow: settle(&mut recordings),
                    });
                }
                Ok(false) => {}
                Err(error) => {
                    qk_executor::status!("qk: {}: remote cache unavailable ({error:#})", task.id)
                }
            }
        }
        // Settled before it runs: recording it merges the path sets fetched.
        let shadow = settle(&mut recordings);
        qk_executor::report::event(qk_executor::report::Event::Cache {
            id: task.id.clone(),
            cached: qk_executor::report::Cached::Miss,
        });
        let log = match tempfile::NamedTempFile::new_in(self.root.join("tmp")) {
            Ok(log) => log,
            Err(error) => return bypass(format!("cache log unavailable: {error}")),
        };
        let capture =
            Capture::new(Some(log.as_file().try_clone()?), prepared.display.clone()).retain_log();
        let restored = before(running);
        let listed = match (&recordings, &prepared.recording) {
            (Some(_), Some(_)) => {
                let project_root = workspace
                    .projects
                    .get(&task.project)
                    .map_or("", |project| project.root.as_str());
                recorded::Before::take(
                    &workspace.root,
                    unlisted.directories(project_root),
                    &unlisted,
                )
            }
            _ => recorded::Before::default(),
        };
        let started = recorded::now(&self.root.join("tmp"));
        let executing = profile::span(&task.id, "execute");
        let outcome = execute_captured(running, cancelled, Some(&capture))?;
        drop(executing);
        let warm_report = after(outcome, restored, running);
        let inputs_unchanged =
            self.inputs_match(snapshot, workspace, graph, task, prepared, &key, cancelled);
        let execution = capture.take_log().map(|log| Execution {
            log,
            inputs_unchanged,
        });
        let mut recorded = unrecorded.map(|unrecorded| match &recordings {
            Some((sets, _, _)) if !sets.is_empty() => Recorded::Missed,
            _ => unrecorded,
        });
        let path_set = match (&recordings, &prepared.recording) {
            (Some((_, _, settings)), Some(recording)) => Some(
                self.path_set(workspace, recording, prepared, settings, &own)
                    .and_then(|set| {
                        // Keyed by nothing, a recording would match any workspace.
                        if set.is_empty() {
                            return Err("its trace named no path in the workspace".to_owned());
                        }
                        match set
                            .changed_since(&workspace.root, started, &unlisted, &listed)
                            .map(|changed| changed.to_string())
                        {
                            Some(reason) => Err(reason),
                            None => Ok(set),
                        }
                    }),
            ),
            _ => None,
        };
        if let Some(Err(reason)) = &path_set {
            qk_executor::status!("qk: {}: not recorded ({reason})", task.id);
            recorded = Some(Recorded::Failed {
                reason: reason.clone(),
            });
        }
        if outcome == Outcome::Success && inputs_unchanged && capture.healthy() {
            let publishing = profile::span(&task.id, "publish");
            let published = self.publish(&workspace.root, &key, &outputs, log.path());
            drop(publishing);
            match published {
                Ok(()) => {
                    store::record_outputs(&workspace.root, &task.id, &key, &outputs);
                    if let Some(remote) = &self.remote {
                        remote.upload(&self.root, &key);
                    }
                }
                Err(error) => {
                    qk_executor::status!("qk: {}: could not save cache entry ({error})", task.id);
                }
            }
            // Its result is kept under the key of what it read, too.
            if let (Some((_, weak, _)), Some(Ok(set))) = (&recordings, path_set) {
                let saved = recorded::strong_key(weak, &set, snapshot, &workspace.root, &unlisted)
                    .and_then(|strong| {
                        self.publish(&workspace.root, &strong, &outputs, log.path())?;
                        recorded::save(&self.root, weak, set)?;
                        // Shared, the sets are merged with those the store
                        // held when this task looked it up.
                        if let Some(remote) = &self.remote {
                            remote.upload(&self.root, &strong);
                            if let Some(record) = recorded::record(&self.root, weak) {
                                remote.upload_path_sets(&self.root, weak, record);
                            }
                        }
                        Ok(strong)
                    });
                recorded = Some(match saved {
                    Ok(key) => Recorded::Saved { key },
                    Err(error) => {
                        qk_executor::status!("qk: {}: recording not saved ({error:#})", task.id);
                        Recorded::Failed {
                            reason: format!("{error:#}"),
                        }
                    }
                });
            }
        }
        Ok(TaskResult {
            outcome,
            cache: CacheStatus::Miss,
            key: keyed,
            warm: warm_report,
            execution,
            recorded,
            shadow,
        })
    }

    /// Looks a task up by its recordings and restores what it finds: by each
    /// path set's key in the local cache, then, with none there, among the
    /// path sets and results the remote store holds, adding its path sets to
    /// `sets`.
    #[allow(clippy::too_many_arguments)]
    fn by_recordings(
        &self,
        workspace: &Workspace,
        task: &Task,
        prepared: &PreparedTask,
        snapshot: &hash::Snapshot,
        outputs: &paths::Outputs,
        unlisted: &recorded::Unlisted,
        sets: &mut Vec<recorded::PathSet>,
        weak: &str,
        declared: &str,
        cancelled: &AtomicBool,
    ) -> Found {
        let strong =
            |set: &recorded::PathSet| recording_key(weak, set, snapshot, workspace, unlisted, task);
        let restored = |strong: &str, shown| {
            self.restore(
                &workspace.root,
                &task.id,
                &task.dependencies,
                strong,
                outputs,
                &prepared.display,
                shown,
            )
            .unwrap_or_else(|error| {
                qk_executor::status!("qk: {}: ignoring unusable cache entry ({error})", task.id);
                false
            })
        };
        let local = |strong: &str| restored(strong, qk_executor::Shown::LocalCache);
        let mut keys = Vec::new();
        for set in sets.iter() {
            if let Some(strong) = strong(set) {
                if local(&strong) {
                    return Found::Local(strong);
                }
                keys.push(strong);
            }
        }
        let Some(remote) = &self.remote else {
            return Found::Nothing;
        };
        // Without path sets here, a result here under the declared key is
        // the task's, and asking the store for path sets would only delay
        // it, run after run, and have them shadow it.
        if sets.is_empty()
            && self
                .root
                .join("entries")
                .join(format!("{declared}.json"))
                .is_file()
        {
            return Found::Nothing;
        }
        // Path sets other machines recorded, new here.
        let fetched = remote
            .fetch_path_sets(&self.root, weak, &task.id)
            .and_then(|record| match record {
                Some(record) => recorded::merge(&self.root, weak, &record),
                None => Ok(Vec::new()),
            });
        match fetched {
            Ok(added) => {
                for set in added {
                    if let Some(strong) = strong(&set) {
                        if local(&strong) {
                            sets.push(set);
                            return Found::Local(strong);
                        }
                        keys.push(strong);
                    }
                    sets.push(set);
                }
            }
            Err(error) => {
                qk_executor::status!("qk: {}: remote path sets unavailable ({error:#})", task.id)
            }
        }
        for strong in keys {
            let found = remote
                .fetch(&self.root, &strong, &task.id, cancelled)
                .unwrap_or_else(|error| {
                    qk_executor::status!("qk: {}: remote cache unavailable ({error:#})", task.id);
                    false
                })
                && restored(&strong, qk_executor::Shown::RemoteCache);
            if found {
                return Found::Remote(strong);
            }
        }
        Found::Nothing
    }

    /// Starts looking a task in mode `"record"` up by its recordings, without
    /// restoring anything: by each path set's key in the local cache, and with
    /// none there, by fetching the path sets the remote store holds on a
    /// thread of its own, for [`Self::settle_shadow`] to finish.
    fn start_shadow(
        &self,
        workspace: &Workspace,
        task: &Task,
        snapshot: &hash::Snapshot,
        unlisted: &recorded::Unlisted,
        sets: &[recorded::PathSet],
        weak: &str,
    ) -> Shadowing {
        let mut keys = Vec::new();
        for set in sets {
            if let Some(strong) = recording_key(weak, set, snapshot, workspace, unlisted, task) {
                if self.holds_entry(&strong) {
                    return Shadowing::Settled(Shadow::WouldHit { key: strong });
                }
                keys.push(strong);
            }
        }
        let Some(remote) = &self.remote else {
            return Shadowing::Settled(if sets.is_empty() {
                Shadow::Unrecorded
            } else {
                Shadow::WouldMiss
            });
        };
        let (remote, root, weak, id) = (
            std::sync::Arc::clone(remote),
            self.root.clone(),
            weak.to_owned(),
            task.id.clone(),
        );
        Shadowing::Fetching {
            keys,
            fetch: std::thread::spawn(move || remote.fetch_path_sets(&root, &weak, &id)),
        }
    }

    /// What a task's recordings would have done: [`Self::start_shadow`]'s
    /// lookup, finished with the path sets the store held, which are added
    /// to `sets`, and the results it holds.
    #[allow(clippy::too_many_arguments)]
    fn settle_shadow(
        &self,
        shadowing: Shadowing,
        workspace: &Workspace,
        task: &Task,
        snapshot: &hash::Snapshot,
        unlisted: &recorded::Unlisted,
        sets: &mut Vec<recorded::PathSet>,
        weak: &str,
    ) -> Shadow {
        let (mut keys, fetch) = match shadowing {
            Shadowing::Settled(shadow) => return shadow,
            Shadowing::Fetching { keys, fetch } => (keys, fetch),
        };
        let fetched = fetch
            .join()
            .unwrap_or_else(|_| Err(anyhow::anyhow!("fetching them panicked")))
            .and_then(|record| match record {
                Some(record) => recorded::merge(&self.root, weak, &record),
                None => Ok(Vec::new()),
            });
        match fetched {
            Ok(added) => {
                for set in added {
                    if let Some(strong) =
                        recording_key(weak, &set, snapshot, workspace, unlisted, task)
                    {
                        if self.holds_entry(&strong) {
                            sets.push(set);
                            return Shadow::WouldHit { key: strong };
                        }
                        keys.push(strong);
                    }
                    sets.push(set);
                }
            }
            Err(error) => {
                qk_executor::status!("qk: {}: remote path sets unavailable ({error:#})", task.id)
            }
        }
        let remote = self
            .remote
            .as_ref()
            .expect("only a remote store is fetched from");
        keys.into_iter()
            .find(|strong| remote.holds(strong).unwrap_or(false))
            .map_or(
                if sets.is_empty() {
                    Shadow::Unrecorded
                } else {
                    Shadow::WouldMiss
                },
                |key| Shadow::WouldHit { key },
            )
    }

    /// Whether the local cache holds a result under `key`.
    fn holds_entry(&self, key: &str) -> bool {
        self.root
            .join("entries")
            .join(format!("{key}.json"))
            .is_file()
    }

    /// The path set of a task's recording, or why there is none.
    fn path_set(
        &self,
        workspace: &Workspace,
        recording: &qk_executor::Recording,
        prepared: &PreparedTask,
        settings: &ultracache::Settings,
        own: &[&paths::Outputs],
    ) -> std::result::Result<recorded::PathSet, String> {
        let failed = |error: anyhow::Error| format!("{error:#}");
        let canonical = workspace
            .root
            .canonicalize()
            .map_err(|e| failed(e.into()))?;
        // Only what a path set can hold is kept and counted: a build reads
        // far more in node_modules and outside the workspace than in it.
        let keep = |path: &std::path::Path| {
            [&workspace.root, &canonical].iter().any(|root| {
                path.strip_prefix(root).is_ok_and(|relative| {
                    !relative.components().any(|component| {
                        matches!(
                            component.as_os_str().to_str(),
                            Some("node_modules" | ".git")
                        )
                    })
                })
            })
        };
        let limits = qk_input_analysis::strace::Limits {
            records: 1_000_000,
            bytes: 256 * 1024 * 1024,
            accesses: 500_000,
            keep: Some(&keep),
        };
        let (accesses, diagnostics) =
            qk_input_analysis::strace::collect_within(&recording.directory, &prepared.cwd, &limits)
                .map_err(failed)?;
        // A partial trace would key the task by part of what it read.
        if let Some(diagnostic) = diagnostics.first() {
            return Err(diagnostic.clone());
        }
        let ignore = settings
            .ignore_reads
            .iter()
            .map(|glob| Pattern::new(glob, false))
            .collect::<Result<Vec<_>>>()
            .map_err(failed)?;
        // qk's own directories, where they lie inside the workspace.
        let state = [self.root.clone(), worktree_state(&workspace.root)]
            .iter()
            .filter_map(|path| {
                [&workspace.root, &canonical]
                    .iter()
                    .find_map(|root| path.strip_prefix(root).ok())
                    .and_then(|relative| relative.to_str())
                    .map(|relative| relative.replace('\\', "/"))
            })
            .collect();
        let exclusions = recorded::Exclusions {
            roots: [&workspace.root, &canonical],
            outputs: own,
            ignore,
            state,
        };
        Ok(recorded::PathSet::from_accesses(
            &accesses,
            &exclusions,
            &canonical,
        ))
    }

    /// Rechecks declared inputs for successful and failed executions alike.
    #[allow(clippy::too_many_arguments)]
    fn inputs_match(
        &self,
        snapshot: &hash::Snapshot,
        workspace: &Workspace,
        graph: &TaskGraph,
        task: &Task,
        prepared: &PreparedTask,
        before: &str,
        cancelled: &AtomicBool,
    ) -> bool {
        if cancelled.load(Ordering::SeqCst) {
            return false;
        }
        let _profile = profile::span(&task.id, "inputs_recheck");
        let after = hash::recheck_dependency_keys(snapshot, workspace, graph, task, cancelled)
            .and_then(|dependencies| {
                hash::fingerprint(
                    snapshot,
                    workspace,
                    task,
                    prepared,
                    &dependencies,
                    cancelled,
                )
            });
        if after.as_deref().ok() != Some(before) {
            if task.definition.cache == Some(true) {
                if self.reuse {
                    qk_executor::status!(
                        "qk: {}: not caching because inputs changed during execution",
                        task.id
                    );
                } else {
                    qk_executor::status!(
                        "qk: {}: excluding execution from flaky results because inputs changed during execution",
                        task.id
                    );
                }
            }
            return false;
        }
        true
    }
}

/// A task's key, and the inputs it is computed from, or why it has none.
pub type Keyed = std::result::Result<(String, Value), String>;

/// The key each task in `graph` would be cached under if it ran now, and what
/// it is computed from, without running a task. Runtime inputs are executed,
/// and the dependency outputs a task reads are taken as they are on disk.
pub fn keys(
    workspace: &Workspace,
    graph: &TaskGraph,
    prepared: &BTreeMap<String, PreparedTask>,
    cancelled: &AtomicBool,
) -> Result<BTreeMap<String, Keyed>> {
    let snapshot = hash::Snapshot::new(workspace, graph, &paths::cache_location(workspace))?;
    Ok(graph
        .tasks
        .values()
        .map(|task| {
            let keyed = key_now(&snapshot, workspace, graph, task, prepared, cancelled);
            (task.id.clone(), keyed)
        })
        .collect())
}

fn key_now(
    snapshot: &hash::Snapshot,
    workspace: &Workspace,
    graph: &TaskGraph,
    task: &Task,
    prepared: &BTreeMap<String, PreparedTask>,
    cancelled: &AtomicBool,
) -> Keyed {
    let failed = |error: anyhow::Error| format!("{}: {error:#}", task.id);
    let dependencies =
        hash::dependency_keys(snapshot, workspace, graph, task, cancelled).map_err(failed)?;
    let inputs = hash::inputs(
        snapshot,
        workspace,
        task,
        &prepared[&task.id],
        &dependencies,
        cancelled,
    )
    .map_err(failed)?;
    Ok((hash::key(&inputs).map_err(failed)?, inputs))
}

/// Runs the private detached-upload protocol used by the qk executable.
#[doc(hidden)]
pub fn run_upload_worker() -> anyhow::Result<()> {
    remote::background::run()
}

/// The declared outputs of every target in the workspace, as their targets
/// declare them and as each of their configurations does with its options,
/// which can move an output; one whose outputs cannot be read is left out.
fn workspace_outputs(workspace: &Workspace) -> recorded::WorkspaceOutputs {
    let mut outputs = Vec::new();
    for (name, project) in &workspace.projects {
        for (target, definition) in &project.targets {
            let configured = definition
                .configurations
                .iter()
                .map(|(configuration, options)| {
                    let mut definition = definition.clone();
                    definition.options.extend(options.clone());
                    (Some(configuration.clone()), definition)
                });
            for (configuration, definition) in
                std::iter::once((None, definition.clone())).chain(configured)
            {
                let task = Task {
                    id: format!("{name}:{target}"),
                    project: name.clone(),
                    target: target.clone(),
                    configuration: configuration.clone(),
                    args: Vec::new(),
                    definition,
                    dependencies: Default::default(),
                    edges: Default::default(),
                };
                if let Some(declared) = paths::Outputs::new(workspace, &task)
                    .ok()
                    .filter(paths::Outputs::declared)
                {
                    outputs.push(((name.clone(), target.clone(), configuration), declared));
                }
            }
        }
    }
    let roots = workspace
        .projects
        .iter()
        .map(|(name, project)| (name.clone(), project.root.clone()))
        .collect();
    recorded::WorkspaceOutputs::new(outputs, &roots)
}

/// The targets of every task `task` depends on, each with the configuration
/// it runs with. Its own target is not among them: its outputs are left out
/// of its listings as other targets' are.
fn upstream(graph: &TaskGraph, task: &Task) -> BTreeSet<recorded::TargetOf> {
    let of = |task: &Task| {
        (
            task.project.clone(),
            task.target.clone(),
            task.configuration.clone(),
        )
    };
    let mut targets = BTreeSet::new();
    let mut seen = BTreeSet::new();
    let mut pending: Vec<&String> = task.dependencies.iter().collect();
    while let Some(id) = pending.pop() {
        if !seen.insert(id) {
            continue;
        }
        if let Some(dependency) = graph.tasks.get(id) {
            targets.insert(of(dependency));
            pending.extend(&dependency.dependencies);
        }
    }
    targets
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_blob_writes_keep_results_ordered_and_repair_corruption() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path();
        let cache = Cache::new(root.join("cache"));
        cache.initialize().unwrap();
        let mut sources = Vec::new();
        for index in 0..130 {
            let source = root.join(format!("source-{index}"));
            std::fs::write(&source, format!("content-{}", index % 65)).unwrap();
            sources.push(source);
        }
        let digest = cache.put_blob(&sources[0]).unwrap();
        std::fs::write(cache.root.join("blobs").join(&digest), "corrupt").unwrap();
        std::fs::remove_file(&sources[31]).unwrap();
        let results = cache.put_blobs(&sources);
        assert_eq!(results.len(), sources.len());
        for (index, result) in results.into_iter().enumerate() {
            if index == 31 {
                assert_eq!(
                    result
                        .unwrap_err()
                        .downcast_ref::<std::io::Error>()
                        .unwrap()
                        .kind(),
                    std::io::ErrorKind::NotFound
                );
                continue;
            }
            let digest = result.unwrap();
            let held = cache.root.join("blobs").join(&digest);
            assert_eq!(
                std::fs::read(&held).unwrap(),
                format!("content-{}", index % 65).as_bytes()
            );
            assert_eq!(hash::digest_file(&held).unwrap(), digest);
        }
    }

    /// Content the store holds already is reused, not copied over again.
    #[cfg(unix)]
    #[test]
    fn a_blob_the_store_holds_is_not_written_again() {
        use std::os::unix::fs::MetadataExt;
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path();
        let cache = Cache::new(root.join(".qk/cache"));
        cache.initialize().unwrap();
        std::fs::write(root.join("first"), "same").unwrap();
        std::fs::write(root.join("second"), "same").unwrap();
        std::fs::write(root.join("other"), "different").unwrap();
        let first = cache.put_blob(&root.join("first")).unwrap();
        let blob = cache.root.join("blobs").join(&first);
        let stored = std::fs::metadata(&blob).unwrap().ino();
        assert_eq!(cache.put_blob(&root.join("second")).unwrap(), first);
        assert_eq!(std::fs::metadata(&blob).unwrap().ino(), stored);
        // A stored blob whose content no longer matches is stored anew.
        std::fs::write(&blob, "corrupt").unwrap();
        assert_eq!(cache.put_blob(&root.join("second")).unwrap(), first);
        assert_eq!(std::fs::read(&blob).unwrap(), b"same");
        // So is one that cannot be read.
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&blob, std::fs::Permissions::from_mode(0o000)).unwrap();
        }
        assert_eq!(cache.put_blob(&root.join("second")).unwrap(), first);
        assert_eq!(std::fs::read(&blob).unwrap(), b"same");
        let other = cache.put_blob(&root.join("other")).unwrap();
        assert_ne!(other, first);
        assert_eq!(
            std::fs::read(cache.root.join("blobs").join(other)).unwrap(),
            b"different"
        );
    }

    /// Complete-directory restore preserves contents, links, modes and
    /// timestamps, and the subsequent hit keeps restored files in place.
    #[test]
    fn complete_directory_restores_preserve_outputs() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path();
        std::fs::create_dir_all(root.join("dist/nested/empty")).unwrap();
        std::fs::write(root.join("dist/nested/a.txt"), "a").unwrap();
        std::fs::write(root.join("dist/b.bin"), [0, 1, 2]).unwrap();
        std::fs::create_dir_all(root.join("bundle/empty")).unwrap();
        std::fs::write(root.join("bundle/readonly"), "readonly").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let script = root.join("dist/run.sh");
            std::fs::write(&script, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::os::unix::fs::symlink("nested/a.txt", root.join("dist/link")).unwrap();
            std::fs::set_permissions(
                root.join("dist/nested"),
                std::fs::Permissions::from_mode(0o500),
            )
            .unwrap();
            std::fs::set_permissions(
                root.join("bundle/readonly"),
                std::fs::Permissions::from_mode(0o444),
            )
            .unwrap();
        }
        let log = root.join("log");
        std::fs::write(&log, "").unwrap();

        let cache = Cache::new(root.join(".qk/cache"));
        cache.initialize().unwrap();
        let outputs = paths::Outputs::from_patterns(&["dist", "bundle"]);
        let key = "0".repeat(64);
        cache.publish(root, &key, &outputs, &log).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // The manifest retains the restrictive mode; cleanup needs write
            // permission on the old directory to remove its files.
            std::fs::set_permissions(
                root.join("dist/nested"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
        std::fs::remove_dir_all(root.join("dist")).unwrap();
        std::fs::remove_dir_all(root.join("bundle")).unwrap();
        let before = std::time::SystemTime::now();
        let restored = cache
            .restore(
                root,
                "app:build",
                &Default::default(),
                &key,
                &outputs,
                &qk_executor::Display::Stream,
                qk_executor::Shown::LocalCache,
            )
            .unwrap();
        assert!(restored);
        assert!(root.join("dist/nested/empty").is_dir());
        assert!(root.join("bundle/empty").is_dir());
        assert_eq!(
            std::fs::read_to_string(root.join("bundle/readonly")).unwrap(),
            "readonly"
        );
        #[cfg(unix)]
        {
            assert_eq!(
                std::fs::read_link(root.join("dist/link")).unwrap(),
                std::path::Path::new("nested/a.txt")
            );
            assert_eq!(
                store::mode(&std::fs::metadata(root.join("dist/run.sh")).unwrap()),
                0o755
            );
            assert_eq!(
                store::mode(&std::fs::metadata(root.join("dist/nested")).unwrap()),
                0o500
            );
            assert_eq!(
                store::mode(&std::fs::metadata(root.join("bundle/readonly")).unwrap()),
                0o444
            );
        }
        let path = root.join("dist/nested/a.txt");
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert!(modified >= before);
        let kept = cache
            .restore(
                root,
                "app:build",
                &Default::default(),
                &key,
                &outputs,
                &qk_executor::Display::Hidden,
                qk_executor::Shown::LocalCache,
            )
            .unwrap();
        assert!(kept);
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            modified
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                root.join("dist/nested"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
    }

    #[test]
    fn listings_leave_out_other_targets_outputs_but_the_upstreams() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::write(root.join("nx.json"), "{}").unwrap();
        std::fs::create_dir_all(root.join("app")).unwrap();
        std::fs::create_dir_all(root.join("lib")).unwrap();
        // A project at the workspace root, with an output in a folder of its
        // own and one inside another project.
        std::fs::write(
            root.join("project.json"),
            r#"{"name":"workspace","targets":{"cache":{"command":"true",
                "outputs":["{projectRoot}/.cache/build","{projectRoot}/app/extra/out"]}}}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("app/project.json"),
            r#"{"name":"app","root":"./app","targets":{
                "build":{"command":"true","outputs":["{options.outputPath}"],
                  "options":{"outputPath":"app/dist"},
                  "configurations":{"production":{"outputPath":"app/dist-production"}}},
                "lint":{"command":"true"},
                "cover":{"command":"true","outputs":["{projectRoot}/.coverage/unit"]},
                "cover-browser":{"command":"true","outputs":["{projectRoot}/.coverage/browser"]},
                "wasm":{"command":"true","outputs":["{projectRoot}/target/wasm/release/*.wasm"]},
                "test":{"command":"true","dependsOn":[{"projects":["lib"],"target":"build"}],
                  "configurations":{"production":{}}}}}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("lib/project.json"),
            r#"{"name":"lib","targets":{"build":{"command":"true","outputs":["{options.outputPath}"],
                "options":{"outputPath":"lib/dist"},
                "configurations":{"production":{"outputPath":"lib/dist-production"}}}}}"#,
        )
        .unwrap();
        let workspace = Workspace::load(root).unwrap();
        let graph = qk_taskgraph::TaskGraph::build(
            &workspace,
            &[
                qk_taskgraph::Request::parse("app:lint").unwrap(),
                qk_taskgraph::Request::parse("app:cover").unwrap(),
                qk_taskgraph::Request::parse("app:test").unwrap(),
                qk_taskgraph::Request::parse("app:test:production").unwrap(),
            ],
        )
        .unwrap();
        let outputs = workspace_outputs(&workspace);
        let others = |task: &str| recorded::Others {
            outputs: &outputs,
            upstream: upstream(&graph, &graph.tasks[task]),
        };
        let lint = others("app:lint");
        assert!(lint.matches("app/dist/main.js"));
        assert!(lint.matches("app/dist-production/main.js"));
        assert!(lint.matches("lib/dist"));
        assert!(!lint.matches("app/src/main.ts"));
        // The directories holding an output count as it, not its project's.
        assert!(lint.matches("app/target"));
        assert!(lint.matches("app/target/wasm"));
        assert!(!lint.matches("app"));
        // A root project's folders are top-level, but never another project.
        assert!(lint.matches(".cache"));
        assert!(lint.matches("app/extra"));
        assert!(!lint.matches("lib"));
        // Before the task runs, the directories those folders come and go
        // in are listed, another project's root among them.
        let unlisted = recorded::Unlisted {
            own: Vec::new(),
            others: Some(lint),
            ignore: &[],
        };
        let directories = unlisted.directories("lib");
        for directory in [".", ".cache", "app", "app/extra"] {
            assert!(directories.contains(directory), "{directory}");
        }
        // A dependency's output is the test's input, and only that of the
        // configuration it depends on.
        assert!(!others("app:test").matches("lib/dist"));
        assert!(others("app:test").matches("lib/dist-production"));
        // A folder holding the task's own output and another target's is
        // left out, as both are.
        assert!(others("app:cover").matches("app/.coverage"));
        let production = others("app:test:production");
        assert!(!production.matches("lib/dist-production"));
        assert!(production.matches("lib/dist"));
    }
}
