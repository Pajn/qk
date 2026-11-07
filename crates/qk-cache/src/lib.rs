//! Local content-addressed cache shared by linked Git worktrees.

mod ahead;
mod evict;
pub mod expectations;
mod glob;
mod hash;
mod ignored;
mod inputs;
mod lookup;
mod paths;
pub mod profile;
pub mod projections;
mod record;
pub mod recorded;
mod remote;
pub mod scratch;
mod store;
pub mod ultracache;
mod verified;
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

/// The nx.json settings of the workspace's remote store: its `s3`, as Nx's
/// cache plugin reads it, and its `qk:storeServer`, which Nx never reads, so a
/// workspace can name a store server for qk while Nx's plugin keeps `s3`.
fn remote_settings(workspace: &Workspace) -> (Option<&Value>, Option<&Value>) {
    let extra = &workspace.config.extra;
    (extra.get("s3"), extra.get("qk:storeServer"))
}

/// Runs a task the cache does not keep. A sandboxed one's log is kept, which
/// says whether it failed applying a sandbox of its own.
fn execute_uncached(
    task: &PreparedTask,
    cancelled: &AtomicBool,
) -> Result<(Outcome, Option<Execution>)> {
    if task.sandbox.is_none() {
        return Ok((execute(task, cancelled)?, None));
    }
    let capture = Capture::new(None, task.display.clone()).retain_log();
    let outcome = execute_captured(task, cancelled, Some(&capture))?;
    let execution = capture.take_log().map(|log| Execution {
        log,
        inputs_unchanged: false,
    });
    Ok((outcome, execution))
}

/// A task's scratch directories, and all beneath them, as workspace globs:
/// emptied before each run, nothing in them keys a recording.
fn scratch_patterns<'a>(
    root: &'a std::path::Path,
    prepared: &'a PreparedTask,
) -> impl Iterator<Item = Pattern> + 'a {
    prepared.scratch.iter().filter_map(move |scratch| {
        let relative = scratch.directory.strip_prefix(root).ok()?.to_str()?;
        // Named exactly, though its name may hold what a glob reads.
        let named = glob::escape(&relative.replace('\\', "/"));
        Pattern::fileset(&named, false).ok()
    })
}

impl Cache {
    /// What a task's recordings leave out: its `ignoreReads`, and its scratch
    /// directories. The project graph is only built for an entry naming
    /// dependencies.
    fn ignored_reads(
        &self,
        workspace: &Workspace,
        task: &Task,
        prepared: &PreparedTask,
        written: &[String],
    ) -> Result<ultracache::IgnoredReads> {
        let projects = written
            .iter()
            .any(|glob| glob.starts_with('^'))
            .then(|| {
                self.projects
                    .get_or_init(|| qk_graph::ProjectGraph::build(workspace).ok())
                    .as_ref()
            })
            .flatten();
        Ok(
            ultracache::IgnoredReads::new(written, workspace, projects, &task.project)?
                .with_patterns(scratch_patterns(&workspace.root, prepared)),
        )
    }
}

/// The cache for one run; its workspace snapshot is taken on first use.
pub struct Cache {
    pub root: PathBuf,
    snapshot: OnceLock<std::result::Result<hash::Snapshot, String>>,
    remote: Option<std::sync::Arc<remote::Remote>>,
    /// Whether results and warm state may be restored or published.
    reuse: bool,
    /// Whether each task runs from scratch: no result is restored, its warm
    /// paths are emptied rather than restored, and what it gives is saved as
    /// ever, replacing what was saved before.
    cold: bool,
    /// The warm groups `cold` emptied this run, which tasks sharing one fill
    /// together.
    cleared: std::sync::Mutex<std::collections::BTreeSet<String>>,
    /// Remote warm saves fetched ahead, by warm identity.
    prefetches: OnceLock<warm::Prefetches>,
    /// The blobs hashed since the machine started, which restores need not
    /// hash again.
    verified: OnceLock<verified::Verified>,
    /// The tasks whose outputs this run left exactly as they were.
    kept: std::sync::Mutex<std::collections::BTreeSet<String>>,
    /// Warm state being saved in the background.
    saves: std::sync::Mutex<Vec<std::thread::JoinHandle<()>>>,
    cleanup: std::sync::Mutex<Option<store::Cleanup>>,
    /// Every target's declared outputs, which recordings' listings leave out.
    workspace_outputs: OnceLock<recorded::WorkspaceOutputs>,
    /// The project graph, for `ignoreReads` entries that name dependencies;
    /// built on first use, `None` where it cannot be.
    projects: OnceLock<Option<qk_graph::ProjectGraph>>,
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
    WouldHit {
        key: String,
        /// For a task that ran all the same, whether it gave that result.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        check: Option<Check>,
    },
    /// None of its path sets' keys had a result.
    WouldMiss {
        /// For a task its declared inputs took from the cache, the paths of
        /// its newest path set that are not as they were when it was
        /// recorded, as `<path> (<how>)`, `<how>` ending in `, declared`
        /// where its declared inputs cover the path. A change they do not
        /// cover either matters, so they missed it, or does not, and its
        /// recording keys what it need not. At most [`UNCOVERED`].
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        changed: Vec<String>,
        /// Where the newest path set keeps no record of how its paths were,
        /// what it holds that the declared inputs leave out, as
        /// `<path> (<how>)`: one of them changed. At most [`UNCOVERED`].
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        uncovered: Vec<String>,
        /// For a task its declared inputs took from the cache, whether they
        /// cover every one of its path sets, so that mode `"on"` would have
        /// taken the same result by its declared key.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        declared: bool,
    },
    /// It has no path set yet.
    Unrecorded,
}

/// How many uncovered paths a [`Shadow::WouldMiss`] names.
pub const UNCOVERED: usize = 5;

/// Whether a task that ran where a recording would have taken a result from
/// the cache gave that result: it succeeded, with the same output files.
/// Its terminal output is not compared, since timings and the like differ
/// from run to run.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Check {
    pub matched: bool,
    /// How the run differed, as `failed (exit <code>)` or `<path> (<how>)`,
    /// at most [`UNCOVERED`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub differences: Vec<String>,
    /// How many outputs, files and directories of the run or of the recorded
    /// result, its `uncheckedOutputs` left out of the comparison.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub unchecked: usize,
}

fn is_zero(count: &usize) -> bool {
    *count == 0
}

/// A lookup by a task's recordings in mode `"record"`, started before its
/// lookup by its declared key so that the two wait on the remote store
/// together rather than one after the other.
enum Shadowing {
    /// Answered without the store: a recording's result is held here, or
    /// there is no store to ask.
    Settled(Shadow),
    /// Waiting for the path sets the store holds, with the lookup of the
    /// path sets held here, none of which has a result here.
    Fetching {
        search: lookup::Search,
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

/// A lookup's results restored as they are found, here or from the remote
/// store; an entry that cannot be restored is ignored, and said to be.
struct Restoring<'a> {
    cache: &'a Cache,
    workspace: &'a Workspace,
    task: &'a Task,
    prepared: &'a PreparedTask,
    outputs: &'a paths::Outputs,
    cancelled: &'a AtomicBool,
}

impl Restoring<'_> {
    fn restored(&self, key: &str, shown: qk_executor::Shown) -> bool {
        self.cache
            .restore(
                &self.workspace.root,
                &self.task.id,
                &self.task.dependencies,
                key,
                self.outputs,
                &self.prepared.display,
                shown,
            )
            .unwrap_or_else(|error| {
                qk_executor::status!(
                    "qk: {}: ignoring unusable cache entry ({error})",
                    self.task.id
                );
                false
            })
    }
}

impl lookup::Results for Restoring<'_> {
    fn here(&self, key: &str) -> bool {
        self.restored(key, qk_executor::Shown::LocalCache)
    }

    fn there(&self, key: &str) -> bool {
        let Some(remote) = &self.cache.remote else {
            return false;
        };
        remote
            .fetch(&self.cache.root, key, &self.task.id, self.cancelled)
            .unwrap_or_else(|error| {
                qk_executor::status!("qk: {}: remote cache unavailable ({error:#})", self.task.id);
                false
            })
            && self.restored(key, qk_executor::Shown::RemoteCache)
    }
}

/// A lookup's results only looked for, which restores nothing: here, an
/// entry restoring would take, as far as can be told without restoring it;
/// in the remote store, any it holds.
struct Checking<'a> {
    cache: &'a Cache,
    task: &'a Task,
    outputs: &'a paths::Outputs,
}

impl lookup::Results for Checking<'_> {
    fn here(&self, key: &str) -> bool {
        self.cache
            .usable(&self.task.id, key, self.outputs)
            .unwrap_or(false)
    }

    fn there(&self, key: &str) -> bool {
        let Some(remote) = &self.cache.remote else {
            return false;
        };
        remote.holds(key).unwrap_or(false)
    }
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
            cold: false,
            cleared: Default::default(),
            prefetches: OnceLock::new(),
            verified: OnceLock::new(),
            kept: Default::default(),
            saves: Default::default(),
            cleanup: Default::default(),
            workspace_outputs: OnceLock::new(),
            projects: OnceLock::new(),
        }
    }

    /// The blobs hashed since the machine started, read on first use.
    pub(crate) fn verified(&self) -> &verified::Verified {
        self.verified
            .get_or_init(|| verified::Verified::load(&self.root))
    }

    /// Records verified executions without configuring or using result stores.
    pub fn for_observation(workspace: &Workspace) -> Self {
        Self {
            reuse: false,
            ..Self::new(paths::cache_location(workspace))
        }
    }

    /// The workspace's cache, with the remote store its nx.json `s3` or
    /// `qk:storeServer` key configures, if any. Credentials
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

    /// This cache with every task run from scratch, as `--cold` asks: see
    /// [`Cache::cold`]'s field.
    pub fn cold(self) -> Self {
        Self { cold: true, ..self }
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
        self.finish_prefetches();
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
        // Cold: the groups left as they were, whose save would keep what
        // the run was to replace.
        let uncleared = std::sync::Mutex::new(std::collections::BTreeSet::new());
        let before = |running: &PreparedTask| -> (Vec<warm::Restored>, Vec<String>) {
            let (mut restored, mut present) = (Vec::new(), Vec::new());
            for warm in &entries {
                if self.cold {
                    // Started from nothing, so its save replaces a bad one.
                    if let Err(error) = self.clear_warm(workspace, task, warm) {
                        qk_executor::status!(
                            "qk: {}: warm state{} not cleared, so not saved ({error:#})",
                            task.id,
                            entry_label(warm)
                        );
                        uncleared.lock().unwrap().insert(warm.identity.clone());
                    }
                    continue;
                }
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
            let uncleared = uncleared.lock().unwrap();
            for warm in entries
                .iter()
                .filter(|warm| outcome == Outcome::Success && !uncleared.contains(&warm.identity))
            {
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
            let (outcome, execution) = execute_uncached(prepared, cancelled)?;
            Ok(TaskResult {
                execution,
                ..TaskResult::uncached(outcome)
            })
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
            let (outcome, execution) = execute_uncached(running, cancelled)?;
            drop(executing);
            let warm_report = after(outcome, restored, running);
            return Ok(TaskResult {
                outcome,
                cache: CacheStatus::Uncached,
                key: keyed,
                warm: warm_report,
                execution,
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
        let ignore = settings
            .as_ref()
            .and_then(|settings| {
                self.ignored_reads(workspace, task, prepared, &settings.ignore_reads)
                    .ok()
            })
            .unwrap_or_default();
        let unlisted = recorded::Unlisted {
            own: own.clone(),
            others,
            ignore: &ignore,
        };
        let inputs = &keyed.as_ref().expect("a cacheable task is keyed").1;
        // Why its recordings cannot key it, with its mode, where they cannot.
        let mut unkeyable = None;
        let mut recordings = settings.and_then(|settings| {
            let weak = hash::weak_key(
                snapshot,
                workspace,
                graph,
                task,
                prepared,
                inputs,
                &settings.ignore_reads,
                &ignore.scope(),
                cancelled,
            );
            match weak {
                Ok(weak) => Some((recorded::load(&self.root, &weak), weak, settings)),
                Err(error) => {
                    qk_executor::status!(
                        "qk: {}: not keyed by its recordings ({error:#})",
                        task.id
                    );
                    unkeyable = Some((settings.mode, format!("{error:#}")));
                    None
                }
            }
        });
        let mut shadowing = None;
        // Cold: the keys this task's path sets give, whose entries a later
        // run looks up first, so its result replaces theirs too.
        let mut replaced = Vec::new();
        if let Some((sets, weak, settings)) = &mut recordings {
            match lookup::lookup(settings.mode, self.cold) {
                // Nothing is restored, so there is nothing to look up.
                lookup::Lookup::Replace => {
                    replaced = sets
                        .iter()
                        .filter_map(|set| {
                            recording_key(weak, set, snapshot, workspace, &unlisted, task)
                        })
                        .collect();
                }
                lookup::Lookup::Shadow => {
                    let _lookup = profile::span(&task.id, "recorded_lookup");
                    shadowing = Some(self.start_shadow(
                        workspace, task, snapshot, &outputs, &unlisted, sets, weak, &key,
                    ));
                }
                lookup::Lookup::Restore => {
                    let recorded_profile = profile::span(&task.id, "recorded_restore");
                    let found = self.by_recordings(
                        workspace, task, prepared, snapshot, &outputs, &unlisted, sets, weak, &key,
                        cancelled,
                    );
                    drop(recorded_profile);
                    let remote = matches!(found, lookup::Found::Remote(_));
                    if let lookup::Found::Local(strong) | lookup::Found::Remote(strong) = found {
                        if let Some(store) = self.remote.as_ref().filter(|_| !remote) {
                            store.held(&strong);
                        }
                        recorded::touch(&self.root, weak);
                        qk_executor::report::event(qk_executor::report::Event::Cache {
                            id: task.id.clone(),
                            cached: if remote {
                                qk_executor::report::Cached::RemoteHit
                            } else {
                                qk_executor::report::Cached::Hit
                            },
                        });
                        return Ok(TaskResult {
                            outcome: Outcome::Success,
                            cache: if remote {
                                CacheStatus::RemoteHit
                            } else {
                                CacheStatus::LocalHit
                            },
                            key: keyed,
                            warm: None,
                            execution: None,
                            recorded: lookup::recorded(
                                Some(settings.mode),
                                !sets.is_empty(),
                                lookup::Taken::Recording(strong),
                            ),
                            shadow: None,
                        });
                    }
                }
            }
        }
        // What its recordings would have done, once the store has said.
        let mut settle = |recordings: &mut Option<(Vec<recorded::PathSet>, String, _)>| {
            let shadowing = shadowing.take()?;
            let (sets, weak, _) = recordings.as_mut()?;
            let _wait = profile::span(&task.id, "recorded_wait");
            Some(self.settle_shadow(
                shadowing, workspace, task, snapshot, &outputs, &unlisted, sets, weak,
            ))
        };
        // A task with path sets is keyed by them alone, unless its declared
        // inputs, as they are now, cover what each of them read.
        let mode = recordings.as_ref().map(|(_, _, settings)| settings.mode);
        let had_sets = |recordings: &Option<(Vec<recorded::PathSet>, String, _)>| {
            recordings
                .as_ref()
                .is_some_and(|(sets, _, _)| !sets.is_empty())
        };
        let declared = lookup::by_declared(
            mode,
            self.cold,
            recordings.as_ref().map_or(&[], |(sets, _, _)| sets),
            |sets| {
                hash::coverage(snapshot, workspace, graph, task, prepared, cancelled)
                    .is_ok_and(|coverage| self.covers_all(&coverage, snapshot, workspace, sets))
            },
        );
        // How its recordings keyed it, or why they could not.
        let verdict = |had_sets: bool, taken: lookup::Taken| match &unkeyable {
            Some((mode, reason)) => lookup::unkeyable(*mode, reason, taken),
            None => lookup::recorded(mode, had_sets, taken),
        };
        let unrecorded = verdict(had_sets(&recordings), lookup::Taken::Declared(key.clone()));
        // An entry fetched ahead of this task is local, but it came from the
        // remote store, and is reported as such.
        let prefetched = declared
            && self.remote.as_ref().is_some_and(|remote| {
                remote.resolved(&key);
                remote.prefetched(&key)
            });
        let local_profile = profile::span(&task.id, "local_restore");
        let restored = if declared && !self.cold {
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
                let shadow = settle(&mut recordings);
                let shadow = self.with_changes(
                    shadow,
                    recordings.as_ref(),
                    (snapshot, workspace, graph, task, prepared, &unlisted),
                    cancelled,
                );
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
                    shadow,
                });
            }
            Ok(false) => {}
            Err(error) => {
                qk_executor::status!("qk: {}: ignoring unusable cache entry ({error})", task.id)
            }
        }
        drop(local_profile);
        if let Some(remote) = self.remote.as_ref().filter(|_| declared && !self.cold) {
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
                    let shadow = settle(&mut recordings);
                    let shadow = self.with_changes(
                        shadow,
                        recordings.as_ref(),
                        (snapshot, workspace, graph, task, prepared, &unlisted),
                        cancelled,
                    );
                    return Ok(TaskResult {
                        outcome: Outcome::Success,
                        cache: CacheStatus::RemoteHit,
                        key: keyed,
                        warm: None,
                        execution: None,
                        recorded: unrecorded,
                        shadow,
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
        // Only a recording `--record` made keys it: one made to audit the task
        // is no decision to key results by what it read.
        let keying = prepared
            .recording
            .as_ref()
            .filter(|recording| recording.keys());
        let listed = match (&recordings, keying) {
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
        let path_set = match (&recordings, keying) {
            (Some((_, _, settings)), Some(recording)) => Some(
                self.path_set(workspace, task, recording, prepared, settings, &own)
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
        }
        let mut published = false;
        let kept = outcome == Outcome::Success && inputs_unchanged && capture.healthy();
        if kept {
            let publishing = profile::span(&task.id, "publish");
            let publish = self.publish(&workspace.root, &key, &outputs, log.path());
            drop(publishing);
            match publish {
                Ok(()) => {
                    published = true;
                    store::record_outputs(&workspace.root, &task.id, &key, &outputs);
                    if let Some(remote) = &self.remote {
                        remote.upload(&self.root, &key);
                    }
                    for strong in &replaced {
                        if let Err(error) =
                            self.publish(&workspace.root, strong, &outputs, log.path())
                        {
                            qk_executor::status!(
                                "qk: {}: could not replace a recorded cache entry ({error})",
                                task.id
                            );
                        }
                    }
                }
                Err(error) => {
                    qk_executor::status!("qk: {}: could not save cache entry ({error})", task.id);
                }
            }
        }
        // Ran where a recording would have taken a result from the cache:
        // whether it gave that result, read before this run's own recording
        // can replace it.
        let check = match &shadow {
            Some(Shadow::WouldHit { key: recorded, .. }) => match outcome {
                Outcome::Failed(code) => Some(Check {
                    matched: false,
                    differences: vec![format!("failed (exit {code})")],
                    unchecked: 0,
                }),
                Outcome::Success if published => {
                    let _checking = profile::span(&task.id, "recorded_check");
                    let settings = recordings.as_ref().map(|(_, _, settings)| settings);
                    self.check(&task.id, &key, recorded, settings)
                }
                _ => None,
            },
            _ => None,
        };
        let ran = match (path_set, &recordings) {
            (Some(Err(reason)), _) => lookup::Ran::Refused(reason),
            // Its result is kept under the key of what it read, too.
            (Some(Ok(set)), Some((_, weak, _))) if kept => {
                let saved = recorded::with_values(weak, set, snapshot, &workspace.root, &unlisted)
                    .and_then(|(strong, set)| {
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
                match saved {
                    Ok(key) => lookup::Ran::Saved(key),
                    Err(error) => {
                        qk_executor::status!("qk: {}: recording not saved ({error:#})", task.id);
                        lookup::Ran::Unsaved(format!("{error:#}"))
                    }
                }
            }
            (Some(Ok(_)), _) => {
                // A task that failed says so itself.
                let why = if outcome != Outcome::Success {
                    None
                } else if !inputs_unchanged {
                    Some("its inputs changed while it ran")
                } else {
                    Some("its log could not be written")
                };
                if let Some(why) = why {
                    qk_executor::status!("qk: {}: recording not saved ({why})", task.id);
                }
                lookup::Ran::Unkept
            }
            (None, _) => match (&unkeyable, keying) {
                // Recorded to key it, by recordings that cannot.
                (Some((_, reason)), Some(_)) => lookup::Ran::Refused(reason.clone()),
                _ => lookup::Ran::Unkeyed,
            },
        };
        let recorded = verdict(had_sets(&recordings), lookup::Taken::Ran(ran));
        let shadow = match shadow {
            Some(Shadow::WouldHit { key: recorded, .. }) => Some(Shadow::WouldHit {
                key: recorded,
                check,
            }),
            shadow => shadow,
        };
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

    /// Whether the result just published under `ran` is the one under
    /// `recorded`, here or in the remote store: `None` when that is not to
    /// be had. What its `uncheckedOutputs` name is not compared.
    fn check(
        &self,
        task: &str,
        ran: &str,
        recorded: &str,
        settings: Option<&ultracache::Settings>,
    ) -> Option<Check> {
        let compared = self
            .artifacts(ran, task)
            .and_then(|ran| Ok(ran.zip(self.artifacts(recorded, task)?)))
            .and_then(|compared| {
                let unchecked = settings.map(ultracache::Settings::unchecked).transpose()?;
                Ok(compared.map(|compared| (compared, unchecked)))
            });
        match compared {
            Ok(Some(((mut ran, mut recorded), unchecked))) => {
                let mut left_out = 0;
                if let Some(unchecked) = unchecked {
                    left_out = ran
                        .keys()
                        .chain(recorded.keys())
                        .filter(|path| unchecked(path))
                        .collect::<BTreeSet<_>>()
                        .len();
                    ran.retain(|path, _| !unchecked(path));
                    recorded.retain(|path, _| !unchecked(path));
                }
                let differences = store::differences(&ran, &recorded, UNCOVERED);
                Some(Check {
                    matched: differences.is_empty(),
                    differences,
                    unchecked: left_out,
                })
            }
            Ok(None) => None,
            Err(error) => {
                qk_executor::status!(
                    "qk: {task}: not compared with its recorded result ({error:#})"
                );
                None
            }
        }
    }

    /// `shadow`, naming what changed in the newest path set since it was
    /// recorded, or what it holds that the declared inputs leave out, when no
    /// recording would have hit.
    fn with_changes(
        &self,
        shadow: Option<Shadow>,
        recordings: Option<&(Vec<recorded::PathSet>, String, ultracache::Settings)>,
        (snapshot, workspace, graph, task, prepared, unlisted): (
            &hash::Snapshot,
            &Workspace,
            &TaskGraph,
            &Task,
            &PreparedTask,
            &recorded::Unlisted,
        ),
        cancelled: &AtomicBool,
    ) -> Option<Shadow> {
        let (Some(Shadow::WouldMiss { .. }), Some((sets, _, _))) = (&shadow, recordings) else {
            return shadow;
        };
        let Some(newest) = sets.first() else {
            return shadow;
        };
        let _explaining = profile::span(&task.id, "recorded_changes");
        let Ok(coverage) = hash::coverage(snapshot, workspace, graph, task, prepared, cancelled)
        else {
            return shadow;
        };
        let declared = self.covers_all(&coverage, snapshot, workspace, sets);
        let outputs = self
            .workspace_outputs
            .get_or_init(|| workspace_outputs(workspace));
        let output = |path: &str| outputs.matches(path);
        let uncovered: BTreeMap<&str, &str> = coverage
            .uncovered(snapshot, &workspace.root, newest, &output)
            .collect();
        let named = |paths: Vec<String>| {
            let more = paths.len().saturating_sub(UNCOVERED - 1);
            let mut paths = paths;
            if more > 1 {
                paths.truncate(UNCOVERED - 1);
                paths.push(format!("{more} more"));
            }
            paths
        };
        if newest.recorded.is_some() {
            let changed = newest
                .changed(snapshot, &workspace.root, unlisted)
                .unwrap_or_default()
                .into_iter()
                .map(|(path, how)| match uncovered.contains_key(path) {
                    true => format!("{path} ({how})"),
                    false => format!("{path} ({how}, declared)"),
                })
                .collect();
            return Some(Shadow::WouldMiss {
                changed: named(changed),
                uncovered: vec![],
                declared,
            });
        }
        let uncovered = uncovered
            .into_iter()
            .map(|(path, how)| format!("{path} ({how})"))
            .collect();
        Some(Shadow::WouldMiss {
            changed: vec![],
            uncovered: named(uncovered),
            declared,
        })
    }

    /// Whether `coverage`, a task's declared key, covers every one of its
    /// path sets: then a task none of whose path sets' keys has a result is
    /// looked up by its declared key.
    fn covers_all(
        &self,
        coverage: &hash::Coverage,
        snapshot: &hash::Snapshot,
        workspace: &Workspace,
        sets: &[recorded::PathSet],
    ) -> bool {
        let outputs = self
            .workspace_outputs
            .get_or_init(|| workspace_outputs(workspace));
        let output = |path: &str| outputs.matches(path);
        sets.iter()
            .all(|set| coverage.covers(snapshot, &workspace.root, set, &output))
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
    ) -> lookup::Found {
        let strong =
            |set: &recorded::PathSet| recording_key(weak, set, snapshot, workspace, unlisted, task);
        let results = Restoring {
            cache: self,
            workspace,
            task,
            prepared,
            outputs,
            cancelled,
        };
        let search = match lookup::Search::here(sets, strong, &results) {
            Ok(found) => return found,
            Err(search) => search,
        };
        let Some(remote) = self.remote.as_ref().filter(|_| {
            lookup::asks_store(lookup::Lookup::Restore, sets, || self.holds_entry(declared))
        }) else {
            return lookup::Found::Nothing;
        };
        let added = self.merged(
            remote.fetch_path_sets(&self.root, weak, &task.id),
            weak,
            task,
        );
        search.there(added, sets, strong, &results)
    }

    /// Starts looking a task in mode `"record"` up by its recordings, without
    /// restoring anything: by each path set's key in the local cache, and with
    /// none there, by fetching the path sets the remote store holds on a
    /// thread of its own, for [`Self::settle_shadow`] to finish.
    #[allow(clippy::too_many_arguments)]
    fn start_shadow(
        &self,
        workspace: &Workspace,
        task: &Task,
        snapshot: &hash::Snapshot,
        outputs: &paths::Outputs,
        unlisted: &recorded::Unlisted,
        sets: &[recorded::PathSet],
        weak: &str,
        declared: &str,
    ) -> Shadowing {
        let strong =
            |set: &recorded::PathSet| recording_key(weak, set, snapshot, workspace, unlisted, task);
        let checking = Checking {
            cache: self,
            task,
            outputs,
        };
        let search = match lookup::Search::here(sets, strong, &checking) {
            Ok(found) => return Shadowing::Settled(lookup::shadow(found, sets)),
            Err(search) => search,
        };
        let Some(remote) = self.remote.as_ref().filter(|_| {
            lookup::asks_store(lookup::Lookup::Shadow, sets, || self.holds_entry(declared))
        }) else {
            return Shadowing::Settled(lookup::unfound(sets));
        };
        let (remote, root, weak, id) = (
            std::sync::Arc::clone(remote),
            self.root.clone(),
            weak.to_owned(),
            task.id.clone(),
        );
        Shadowing::Fetching {
            search,
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
        outputs: &paths::Outputs,
        unlisted: &recorded::Unlisted,
        sets: &mut Vec<recorded::PathSet>,
        weak: &str,
    ) -> Shadow {
        let (search, fetch) = match shadowing {
            Shadowing::Settled(shadow) => return shadow,
            Shadowing::Fetching { search, fetch } => (search, fetch),
        };
        let fetched = fetch
            .join()
            .unwrap_or_else(|_| Err(anyhow::anyhow!("fetching them panicked")));
        let added = self.merged(fetched, weak, task);
        let strong =
            |set: &recorded::PathSet| recording_key(weak, set, snapshot, workspace, unlisted, task);
        let checking = Checking {
            cache: self,
            task,
            outputs,
        };
        let found = search.there(added, sets, strong, &checking);
        lookup::shadow(found, sets)
    }

    /// The path sets of the record the store held, `fetched`, that are new
    /// here, merged into those held here; none when it could not be had.
    fn merged(
        &self,
        fetched: Result<Option<Vec<u8>>>,
        weak: &str,
        task: &Task,
    ) -> Vec<recorded::PathSet> {
        fetched
            .and_then(|record| match record {
                Some(record) => recorded::merge(&self.root, weak, &record),
                None => Ok(Vec::new()),
            })
            .unwrap_or_else(|error| {
                qk_executor::status!("qk: {}: remote path sets unavailable ({error:#})", task.id);
                Vec::new()
            })
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
        task: &Task,
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
        // A path set names what a task read by the paths it named. A partial
        // trace would key the task by part of what it read.
        let limits = qk_input_analysis::strace::Limits::kept(&keep, false);
        let accesses = recording.whole_accesses(&prepared.cwd, &limits)?;
        let ignore = self
            .ignored_reads(workspace, task, prepared, &settings.ignore_reads)
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
/// Each target's scratch directories count as its outputs: they come and go
/// beside another task as outputs do, and nothing in them is kept.
fn workspace_outputs(workspace: &Workspace) -> recorded::WorkspaceOutputs {
    let mut outputs = Vec::new();
    for (name, project) in &workspace.projects {
        for (target, definition) in &project.targets {
            if let Some(scratch) = scratch_outputs(workspace, name, definition) {
                let configurations = std::iter::once(None)
                    .chain(definition.configurations.keys().cloned().map(Some));
                for configuration in configurations {
                    outputs.push((
                        (name.clone(), target.clone(), configuration),
                        scratch.clone(),
                    ));
                }
            }
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

/// A target's scratch directories as outputs; none when it has none, or
/// they cannot be read.
fn scratch_outputs(
    workspace: &Workspace,
    project: &str,
    definition: &qk_config::Target,
) -> Option<paths::Outputs> {
    let directories =
        scratch::directories_of(workspace, project, definition.extra.get("qk:sandbox")).ok()?;
    let patterns: Vec<String> = directories
        .iter()
        .filter_map(|directory| {
            let relative = directory.strip_prefix(&workspace.root).ok()?.to_str()?;
            Some(glob::escape(&relative.replace('\\', "/")))
        })
        .collect();
    if patterns.is_empty() {
        return None;
    }
    paths::Outputs::from_paths(&patterns).ok()
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
    // Those `--exclude-task-dependencies` left unrun among them, so that a
    // recording lists the same in any run.
    graph
        .output_dependencies(task, true)
        .iter()
        .filter_map(|id| graph.keyed_task(id))
        .map(of)
        .collect()
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
                "browser":{"command":"true","qk:sandbox":{"scratch":["{projectRoot}/.vitest"]}},
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
                "qk:sandbox":{"scratch":["{projectRoot}/.scratch"]},
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
        assert!(lint.matches("app/.vitest"));
        assert!(lint.matches("app/.vitest/attachments/diff.png"));
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
            ignore: &ultracache::IgnoredReads::none(),
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
        // A dependency's scratch is left in as its outputs are, whichever
        // configuration it runs with.
        assert!(others("app:lint").matches("lib/.scratch"));
        assert!(!others("app:test").matches("lib/.scratch"));
        assert!(!production.matches("lib/.scratch"));
    }
}
