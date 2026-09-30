//! Local content-addressed cache shared by linked Git worktrees.

mod evict;
mod glob;
mod hash;
mod paths;
mod remote;
mod store;
pub mod warm;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use qk_config::Workspace;
use qk_executor::{Capture, Outcome, PreparedTask, execute, execute_captured};
use qk_taskgraph::{Task, TaskGraph};
use serde_json::{Value, json};

pub use evict::{Pruned, max_size, parse_size, prune};
pub use glob::Pattern;
pub use hash::{Resolved, SourceIgnore, source_files, without_resolution};
pub use paths::{
    Outputs, cache_directory, cache_location, clear_worktree_state, resolved_outputs,
    worktree_state,
};

/// The cache for one run; its workspace snapshot is taken on first use.
pub struct Cache {
    pub root: PathBuf,
    snapshot: OnceLock<std::result::Result<hash::Snapshot, String>>,
    remote: Option<std::sync::Arc<remote::Remote>>,
    /// The tasks whose outputs this run left exactly as they were.
    kept: std::sync::Mutex<std::collections::BTreeSet<String>>,
}

/// A task's output fingerprint for its dependents' keys, or why it has none:
/// `"<task id>: <reason>"` for the task where fingerprinting first failed.
pub type Fingerprint = std::result::Result<String, String>;

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

pub struct TaskResult {
    pub outcome: Outcome,
    pub fingerprint: Fingerprint,
    pub cache: CacheStatus,
    /// The task's key and what it was computed from, when it has one.
    pub key: Option<(String, Value)>,
    /// What warm state did, for targets that keep it.
    pub warm: Option<warm::WarmReport>,
}

impl TaskResult {
    pub fn uncached(outcome: Outcome, reason: String) -> Self {
        Self {
            outcome,
            fingerprint: Err(reason),
            cache: CacheStatus::Uncached,
            key: None,
            warm: None,
        }
    }
}

/// Each task's resolved inputs, for affected selection. `extra` paths are
/// candidates beside the workspace's files, so a deleted file still matches
/// the inputs that named it. Env and runtime inputs are not evaluated.
pub fn resolve_tasks(
    workspace: &Workspace,
    graph: &TaskGraph,
    extra: &[String],
) -> Result<BTreeMap<String, Resolved>> {
    let cache = paths::cache_location(workspace);
    let snapshot = hash::Snapshot::new(workspace, graph, &cache)?.with_candidates(extra);
    let cancelled = AtomicBool::new(false);
    graph
        .tasks
        .iter()
        .map(|(id, task)| {
            let resolved = hash::resolve(&snapshot, workspace, task, None, &cancelled)
                .with_context(|| format!("cannot resolve the inputs of {id}"))?;
            Ok((id.clone(), resolved))
        })
        .collect()
}

/// Every task's resolved inputs, or why they could not be resolved, with the
/// workspace files they were chosen from.
pub struct Resolution {
    pub candidates: std::collections::BTreeSet<String>,
    pub tasks: BTreeMap<String, std::result::Result<Resolved, String>>,
}

pub fn resolve_each(workspace: &Workspace, graph: &TaskGraph) -> Result<Resolution> {
    let cache = paths::cache_location(workspace);
    let snapshot = hash::Snapshot::new(workspace, graph, &cache)?;
    let cancelled = AtomicBool::new(false);
    let tasks = graph
        .tasks
        .iter()
        .map(|(id, task)| {
            let resolved = hash::resolve(&snapshot, workspace, task, None, &cancelled)
                .map_err(|error| format!("{error:#}"));
            (id.clone(), resolved)
        })
        .collect();
    Ok(Resolution {
        candidates: snapshot.files.clone(),
        tasks,
    })
}

impl Cache {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            snapshot: OnceLock::new(),
            remote: None,
            kept: Default::default(),
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
        let remote = match remote::configure(workspace.config.extra.get("s3"), environment) {
            Ok(remote) => remote.map(std::sync::Arc::new),
            Err(error) => {
                qk_executor::status!("qk: remote cache disabled: {error:#}");
                None
            }
        };
        Self {
            remote,
            ..Self::new(paths::cache_location(workspace))
        }
    }

    /// Saves what later runs reuse and waits for background uploads to the
    /// remote store, reporting failures.
    pub fn finish(&self, workspace: &Workspace) {
        if let Some(Ok(snapshot)) = self.snapshot.get() {
            snapshot.save_digests(&workspace.root);
        }
        if let Some(remote) = &self.remote {
            let failures = remote.finish();
            for failure in &failures {
                qk_executor::status!("qk: remote cache upload failed for {failure}");
            }
        }
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
        dependencies: &BTreeMap<String, Fingerprint>,
        cancelled: &AtomicBool,
    ) -> Result<TaskResult> {
        let cacheable = task.definition.cache == Some(true);
        let warm = match warm::config(workspace, task) {
            Ok(warm) => warm,
            Err(error) => {
                qk_executor::status!("qk: {}: warm state ignored ({error:#})", task.id);
                None
            }
        };
        // The warm variables point tools at their state; they are not inputs,
        // so the key is computed from the task without them.
        let mut running = prepared.clone();
        if let Some(warm) = &warm {
            for (name, value) in &warm.env {
                running.execution.insert(name.into(), value.into());
            }
        }
        let before = || -> Option<warm::Restored> {
            let warm = warm.as_ref()?;
            match self
                .initialize()
                .and_then(|()| self.restore_warm(workspace, task, warm))
            {
                Ok(restored) => (!restored.groups.is_empty()).then_some(restored),
                Err(error) => {
                    qk_executor::status!("qk: {}: warm state not restored ({error:#})", task.id);
                    None
                }
            }
        };
        let after = |outcome: Outcome,
                     restored: Option<warm::Restored>|
         -> Option<warm::WarmReport> {
            let warm = warm.as_ref()?;
            let started = std::time::Instant::now();
            let save_ms = if outcome == Outcome::Success {
                match self.save_warm(workspace, task, warm) {
                    Ok(()) => Some(started.elapsed().as_millis() as u64),
                    Err(error) => {
                        qk_executor::status!("qk: {}: warm state not saved ({error:#})", task.id);
                        None
                    }
                }
            } else {
                None
            };
            Some(warm::WarmReport { restored, save_ms })
        };
        let fallback = |reason: String| {
            execute(prepared, cancelled).map(|outcome| TaskResult::uncached(outcome, reason))
        };
        // Reports why this task runs uncached, attributing the reason to this task.
        let bypass = |reason: String| {
            if cacheable {
                qk_executor::status!("qk: {}: cache bypassed ({reason})", task.id);
            }
            fallback(format!("{}: {reason}", task.id))
        };
        let dependencies = match dependencies
            .iter()
            .map(|(id, key)| key.clone().map(|key| (id.clone(), key)))
            .collect::<std::result::Result<BTreeMap<_, _>, _>>()
        {
            Ok(dependencies) => dependencies,
            Err(root) => {
                // Keep the original task and reason so chains stay readable.
                if cacheable {
                    qk_executor::status!("qk: {}: cache bypassed (depends on {root})", task.id);
                }
                return fallback(root);
            }
        };
        let snapshot = match self.snapshot(workspace, graph) {
            Ok(snapshot) => snapshot,
            Err(reason) => return bypass(reason),
        };
        let dependencies =
            match hash::dependency_keys(snapshot, workspace, graph, task, &dependencies, cancelled)
            {
                Ok(dependencies) => dependencies,
                Err(error) => return bypass(format!("{error:#}")),
            };
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
            let restored = before();
            let outcome = execute(running, cancelled)?;
            let warm_report = after(outcome, restored);
            let fingerprint = if self.unchanged(
                snapshot,
                workspace,
                task,
                prepared,
                &dependencies,
                &key,
                outcome,
                cancelled,
            ) {
                outputs_fingerprint(task, &workspace.root, &outputs, &key)
            } else {
                Err(format!("{}: did not complete unchanged", task.id))
            };
            return Ok(TaskResult {
                outcome,
                fingerprint,
                cache: CacheStatus::Uncached,
                key: keyed,
                warm: warm_report,
            });
        }
        if let Err(error) = self.initialize() {
            return bypass(format!("cache unavailable: {error}"));
        }
        let (_lock, waited) = match self.lock(&key, cancelled) {
            Ok(Some(lock)) => lock,
            Ok(None) => {
                return Ok(TaskResult::uncached(
                    Outcome::Cancelled,
                    format!("{}: cancelled", task.id),
                ));
            }
            Err(error) => return bypass(format!("cache lock unavailable: {error}")),
        };
        // Inputs may have changed while another worktree held this key's lock.
        if waited
            && hash::fingerprint(
                snapshot,
                workspace,
                task,
                prepared,
                &dependencies,
                cancelled,
            )
            .ok()
            .as_ref()
                != Some(&key)
        {
            return bypass("inputs changed while waiting for another run".into());
        }
        match self.restore(
            &workspace.root,
            &task.id,
            &task.dependencies,
            &key,
            &outputs,
            &prepared.display,
            qk_executor::Shown::LocalCache,
        ) {
            Ok(Some(fingerprint)) => {
                qk_executor::report::event(qk_executor::report::Event::Cache {
                    id: task.id.clone(),
                    cached: qk_executor::report::Cached::Hit,
                });
                return Ok(TaskResult {
                    outcome: Outcome::Success,
                    fingerprint: Ok(fingerprint),
                    cache: CacheStatus::LocalHit,
                    key: keyed,
                    warm: None,
                });
            }
            Ok(None) => {}
            Err(error) => {
                qk_executor::status!("qk: {}: ignoring unusable cache entry ({error})", task.id)
            }
        }
        if let Some(remote) = &self.remote {
            let fetched = remote.fetch(&self.root, &key).and_then(|found| {
                if !found {
                    return Ok(None);
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
                Ok(Some(fingerprint)) => {
                    qk_executor::report::event(qk_executor::report::Event::Cache {
                        id: task.id.clone(),
                        cached: qk_executor::report::Cached::RemoteHit,
                    });
                    return Ok(TaskResult {
                        outcome: Outcome::Success,
                        fingerprint: Ok(fingerprint),
                        cache: CacheStatus::RemoteHit,
                        key: keyed,
                        warm: None,
                    });
                }
                Ok(None) => {}
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
        let capture = Capture::new(Some(log.as_file().try_clone()?), prepared.display.clone());
        let restored = before();
        let outcome = execute_captured(running, cancelled, Some(&capture))?;
        let warm_report = after(outcome, restored);
        let fingerprint = if !self.unchanged(
            snapshot,
            workspace,
            task,
            prepared,
            &dependencies,
            &key,
            outcome,
            cancelled,
        ) {
            Err(format!("{}: did not complete unchanged", task.id))
        } else if !capture.healthy() {
            outputs_fingerprint(task, &workspace.root, &outputs, &key)
        } else {
            // The saved manifest already hashes every output; reuse it.
            match self.publish(&workspace.root, &key, &outputs, log.path()) {
                Ok(fingerprint) => {
                    store::record_outputs(&workspace.root, &task.id, &key, &outputs);
                    if let Some(remote) = &self.remote {
                        remote.upload(&self.root, &key);
                    }
                    Ok(fingerprint)
                }
                Err(error) => {
                    qk_executor::status!("qk: {}: could not save cache entry ({error})", task.id);
                    outputs_fingerprint(task, &workspace.root, &outputs, &key)
                }
            }
        };
        Ok(TaskResult {
            outcome,
            fingerprint,
            cache: CacheStatus::Miss,
            key: keyed,
            warm: warm_report,
        })
    }

    /// Whether a successful task's inputs still match the key it ran under.
    #[allow(clippy::too_many_arguments)]
    fn unchanged(
        &self,
        snapshot: &hash::Snapshot,
        workspace: &Workspace,
        task: &Task,
        prepared: &PreparedTask,
        dependencies: &BTreeMap<String, String>,
        before: &str,
        outcome: Outcome,
        cancelled: &AtomicBool,
    ) -> bool {
        if outcome != Outcome::Success || cancelled.load(Ordering::SeqCst) {
            return false;
        }
        let after = hash::fingerprint(snapshot, workspace, task, prepared, dependencies, cancelled);
        if after.as_deref().ok() != Some(before) {
            if task.definition.cache == Some(true) {
                qk_executor::status!(
                    "qk: {}: not caching because inputs changed during execution",
                    task.id
                );
            }
            return false;
        }
        true
    }
}

/// Per-output values for dependents' keys. Disk and manifest must agree exactly,
/// so a dependent's key is the same whether this task was restored or executed.
pub(crate) fn file_output(content: &str, mode: u32) -> Value {
    json!({"content": content, "mode": mode})
}

pub(crate) fn link_output(target: &str) -> Value {
    json!({"link": target})
}

pub(crate) fn directory_output() -> Value {
    json!("directory")
}

/// What a task's dependents are keyed on. A task that declares outputs is
/// known to them only through those outputs, so a run that reproduces them
/// leaves its dependents cached however its own inputs changed. A task
/// without declared outputs may affect its dependents in ways qk cannot see,
/// so they are keyed on its inputs.
pub(crate) fn combine_outputs(
    input: &str,
    files: &BTreeMap<String, Value>,
    declared: bool,
) -> Result<String> {
    let identity = if declared { "declared outputs" } else { input };
    Ok(blake3::hash(&serde_json::to_vec(&(identity, files))?)
        .to_hex()
        .to_string())
}

fn outputs_fingerprint(
    task: &Task,
    root: &Path,
    outputs: &paths::Outputs,
    input: &str,
) -> Fingerprint {
    output_fingerprint(root, outputs, input)
        .map_err(|error| format!("{}: cannot fingerprint outputs: {error:#}", task.id))
}

fn output_fingerprint(root: &Path, outputs: &paths::Outputs, input: &str) -> Result<String> {
    let mut files = BTreeMap::new();
    for path in outputs.paths(root)? {
        paths::safe_parents(root, &path)?;
        let absolute = root.join(&path);
        let metadata = std::fs::symlink_metadata(&absolute)?;
        let value = if metadata.file_type().is_symlink() {
            link_output(
                std::fs::read_link(absolute)?
                    .to_str()
                    .context("symlink target must be UTF-8")?,
            )
        } else if metadata.is_file() {
            file_output(&hash::digest_file(&absolute)?, store::mode(&metadata))
        } else {
            directory_output()
        };
        files.insert(path, value);
    }
    combine_outputs(input, &files, outputs.declared())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Complete-directory restore preserves contents, links, modes, timestamps
    /// and fingerprints, and the subsequent hit keeps restored files in place.
    #[test]
    fn manifest_and_disk_output_fingerprints_agree() {
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
        let saved = cache.publish(root, &key, &outputs, &log).unwrap();
        assert_eq!(saved, output_fingerprint(root, &outputs, &key).unwrap());

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
        assert_eq!(restored, Some(saved.clone()));
        assert_eq!(saved, output_fingerprint(root, &outputs, &key).unwrap());
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
                Path::new("nested/a.txt")
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
        assert_eq!(kept, Some(saved));
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
