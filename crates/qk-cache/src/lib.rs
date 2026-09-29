//! Local content-addressed cache shared by linked Git worktrees.

mod glob;
mod hash;
mod paths;
mod store;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use qk_config::Workspace;
use qk_executor::{Capture, Outcome, PreparedTask, execute, execute_captured};
use qk_taskgraph::{Task, TaskGraph};
use serde_json::{Value, json};

pub use glob::Pattern;
pub use paths::cache_directory;

/// The cache for one run; its workspace snapshot is taken on first use.
pub struct Cache {
    pub root: PathBuf,
    snapshot: OnceLock<std::result::Result<hash::Snapshot, String>>,
}

/// A task's output fingerprint for its dependents' keys, or why it has none:
/// `"<task id>: <reason>"` for the task where fingerprinting first failed.
pub type Fingerprint = std::result::Result<String, String>;

pub struct TaskResult {
    pub outcome: Outcome,
    pub fingerprint: Fingerprint,
    pub hit: bool,
}

impl TaskResult {
    pub fn uncached(outcome: Outcome, reason: String) -> Self {
        Self {
            outcome,
            fingerprint: Err(reason),
            hit: false,
        }
    }
}

impl Cache {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            snapshot: OnceLock::new(),
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
        let key = match hash::fingerprint(
            snapshot,
            workspace,
            task,
            prepared,
            &dependencies,
            cancelled,
        ) {
            Ok(key) => key,
            Err(error) => return bypass(format!("{error:#}")),
        };
        let outputs = match paths::Outputs::new(workspace, task) {
            Ok(outputs) => outputs,
            Err(error) => return bypass(format!("{error:#}")),
        };
        if !cacheable {
            let outcome = execute(prepared, cancelled)?;
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
                hit: false,
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
        match self.restore(&workspace.root, &key, &outputs) {
            Ok(Some(fingerprint)) => {
                qk_executor::status!("qk: cache hit {}", task.id);
                return Ok(TaskResult {
                    outcome: Outcome::Success,
                    fingerprint: Ok(fingerprint),
                    hit: true,
                });
            }
            Ok(None) => {}
            Err(error) => {
                qk_executor::status!("qk: {}: ignoring unusable cache entry ({error})", task.id)
            }
        }
        qk_executor::status!("qk: cache miss {}", task.id);
        let log = match tempfile::NamedTempFile::new_in(self.root.join("tmp")) {
            Ok(log) => log,
            Err(error) => return bypass(format!("cache log unavailable: {error}")),
        };
        let capture = Capture::new(log.as_file().try_clone()?, true);
        let outcome = execute_captured(prepared, cancelled, Some(&capture))?;
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
                Ok(fingerprint) => Ok(fingerprint),
                Err(error) => {
                    qk_executor::status!("qk: {}: could not save cache entry ({error})", task.id);
                    outputs_fingerprint(task, &workspace.root, &outputs, &key)
                }
            }
        };
        Ok(TaskResult {
            outcome,
            fingerprint,
            hit: false,
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

pub(crate) fn combine_outputs(input: &str, files: &BTreeMap<String, Value>) -> Result<String> {
    Ok(blake3::hash(&serde_json::to_vec(&(input, files))?)
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
    combine_outputs(input, &files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_and_disk_output_fingerprints_agree() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path();
        std::fs::create_dir_all(root.join("dist/nested/empty")).unwrap();
        std::fs::write(root.join("dist/nested/a.txt"), "a").unwrap();
        std::fs::write(root.join("dist/b.bin"), [0, 1, 2]).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let script = root.join("dist/run.sh");
            std::fs::write(&script, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::os::unix::fs::symlink("nested/a.txt", root.join("dist/link")).unwrap();
        }
        let log = root.join("log");
        std::fs::write(&log, "").unwrap();

        let cache = Cache::new(root.join(".qk/cache"));
        cache.initialize().unwrap();
        let outputs = paths::Outputs::from_patterns(&["dist"]);
        let key = "0".repeat(64);
        let saved = cache.publish(root, &key, &outputs, &log).unwrap();
        assert_eq!(saved, output_fingerprint(root, &outputs, &key).unwrap());

        std::fs::remove_dir_all(root.join("dist")).unwrap();
        let restored = cache.restore(root, &key, &outputs).unwrap();
        assert_eq!(restored, Some(saved.clone()));
        assert_eq!(saved, output_fingerprint(root, &outputs, &key).unwrap());
    }
}
