//! Local content-addressed cache shared by linked Git worktrees.

mod evict;
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

use std::collections::BTreeMap;
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
pub use hash::{DependencyGlob, Selects, SourceIgnore, dependency_globs, source_files};
pub use inputs::{
    InputChange, InputChanges, JsonFields, ROOT_TSCONFIG_FILES, Resolution, Resolved, TsConfig,
    manifest_for_dependents, resolve_each, resolve_tasks, selectively_hash_tsconfig,
    without_resolution,
};
/// Expands `{projectRoot}` and `{workspaceRoot}` in a project's path or glob.
pub use paths::expand as expand_project_path;
pub use paths::{
    Outputs, cache_directory, cache_location, clear_worktree_state, resolved_outputs,
    worktree_state,
};

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
        }
    }

    /// Records verified executions without configuring or using result stores.
    pub fn for_observation(workspace: &Workspace) -> Self {
        Self {
            reuse: false,
            ..Self::new(paths::cache_location(workspace))
        }
    }

    /// The workspace's cache, with the remote store its nx.json `s3` key
    /// configures, if any. Credentials and modes are read from `environment`,
    /// which includes the workspace's dotenv files. A remote store that cannot
    /// be used is reported and left out; the local cache still works.
    pub fn for_workspace(
        workspace: &Workspace,
        environment: &BTreeMap<std::ffi::OsString, std::ffi::OsString>,
    ) -> Self {
        let root = paths::cache_location(workspace);
        let remote = match remote::configure(
            workspace.config.extra.get("s3"),
            workspace.config.extra.get("storeServer"),
            environment,
        ) {
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
        let inputs = &keyed.as_ref().expect("a cacheable task is keyed").1;
        let recordings = settings.and_then(|settings| {
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
        if let Some((sets, weak, _)) = &recordings {
            let recorded_profile = profile::span(&task.id, "recorded_restore");
            for set in sets {
                let strong = match recorded::strong_key(weak, set, snapshot, &workspace.root, &own)
                {
                    Ok(strong) => strong,
                    Err(error) => {
                        qk_executor::status!("qk: {}: ignoring a path set ({error:#})", task.id);
                        continue;
                    }
                };
                match self.restore(
                    &workspace.root,
                    &task.id,
                    &task.dependencies,
                    &strong,
                    &outputs,
                    &prepared.display,
                    qk_executor::Shown::LocalCache,
                ) {
                    Ok(true) => {
                        recorded::touch(&self.root, weak);
                        qk_executor::report::event(qk_executor::report::Event::Cache {
                            id: task.id.clone(),
                            cached: qk_executor::report::Cached::Hit,
                        });
                        return Ok(TaskResult {
                            outcome: Outcome::Success,
                            cache: CacheStatus::LocalHit,
                            key: keyed,
                            warm: None,
                            execution: None,
                            recorded: Some(Recorded::Hit { key: strong }),
                        });
                    }
                    Ok(false) => {}
                    Err(error) => qk_executor::status!(
                        "qk: {}: ignoring unusable cache entry ({error})",
                        task.id
                    ),
                }
            }
            drop(recorded_profile);
        }
        // A task with path sets is keyed by them alone: its declared inputs
        // are what a recording found wanting.
        let declared = recordings
            .as_ref()
            .is_none_or(|(sets, _, _)| sets.is_empty());
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
                    recorded: recordings.as_ref().map(|_| Recorded::Unrecorded),
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
                        recorded: recordings.as_ref().map(|_| Recorded::Unrecorded),
                    });
                }
                Ok(false) => {}
                Err(error) => {
                    qk_executor::status!("qk: {}: remote cache unavailable ({error:#})", task.id)
                }
            }
        }
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
        let mut recorded = recordings.as_ref().map(|(sets, _, _)| {
            if sets.is_empty() {
                Recorded::Unrecorded
            } else {
                Recorded::Missed
            }
        });
        let path_set = match (&recordings, &prepared.recording) {
            (Some((_, _, settings)), Some(recording)) => {
                Some(self.path_set(workspace, recording, prepared, settings, &own))
            }
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
                let saved = recorded::strong_key(weak, &set, snapshot, &workspace.root, &own)
                    .and_then(|strong| {
                        self.publish(&workspace.root, &strong, &outputs, log.path())?;
                        recorded::save(&self.root, weak, set)?;
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
        })
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
        let (accesses, diagnostics) =
            qk_input_analysis::strace::collect(&recording.directory, &prepared.cwd)
                .map_err(failed)?;
        // A partial trace would key the task by part of what it read.
        if let Some(diagnostic) = diagnostics.first() {
            return Err(diagnostic.clone());
        }
        let canonical = workspace
            .root
            .canonicalize()
            .map_err(|e| failed(e.into()))?;
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
}
