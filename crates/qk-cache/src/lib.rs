//! Local content-addressed cache shared by linked Git worktrees.

mod hash;
mod paths;
mod store;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use qk_config::Workspace;
use qk_executor::{Capture, Outcome, PreparedTask, execute, execute_captured};
use qk_taskgraph::{Task, TaskGraph};
use serde_json::{Value, json};

pub use paths::cache_directory;

#[derive(Clone)]
pub struct Cache {
    pub root: PathBuf,
}

pub struct TaskResult {
    pub outcome: Outcome,
    pub fingerprint: Option<String>,
    pub hit: bool,
}

impl Cache {
    pub fn run(
        &self,
        workspace: &Workspace,
        graph: &TaskGraph,
        task: &Task,
        prepared: &PreparedTask,
        dependencies: &BTreeMap<String, Option<String>>,
        cancelled: &AtomicBool,
    ) -> Result<TaskResult> {
        let fallback = || {
            execute(prepared, cancelled).map(|outcome| TaskResult {
                outcome,
                fingerprint: None,
                hit: false,
            })
        };
        let Some(dependencies) = dependencies
            .iter()
            .map(|(id, key)| key.clone().map(|key| (id.clone(), key)))
            .collect::<Option<BTreeMap<_, _>>>()
        else {
            if task.definition.cache == Some(true) {
                eprintln!(
                    "qk: {}: cache bypassed (dependency could not be fingerprinted)",
                    task.id
                );
            }
            return fallback();
        };
        let key = match hash::fingerprint(
            workspace,
            graph,
            task,
            prepared,
            &dependencies,
            &self.root,
            cancelled,
        ) {
            Ok(key) => key,
            Err(error) => {
                if task.definition.cache == Some(true) {
                    eprintln!("qk: {}: cache bypassed ({error})", task.id);
                }
                return fallback();
            }
        };
        let outputs = match paths::Outputs::new(workspace, task) {
            Ok(outputs) => outputs,
            Err(error) => {
                if task.definition.cache == Some(true) {
                    eprintln!("qk: {}: cache bypassed ({error})", task.id);
                }
                return fallback();
            }
        };
        if task.definition.cache != Some(true) {
            let outcome = execute(prepared, cancelled)?;
            let fingerprint = self
                .unchanged(
                    workspace,
                    graph,
                    task,
                    prepared,
                    &dependencies,
                    &key,
                    outcome,
                    cancelled,
                )
                .then(|| output_fingerprint(&workspace.root, &outputs, &key).ok())
                .flatten();
            return Ok(TaskResult {
                outcome,
                fingerprint,
                hit: false,
            });
        }
        if let Err(error) = self.initialize() {
            eprintln!("qk: {}: cache unavailable ({error})", task.id);
            return fallback();
        }
        let _lock = match self.lock(&key, cancelled) {
            Ok(Some(lock)) => lock,
            Ok(None) => {
                return Ok(TaskResult {
                    outcome: Outcome::Cancelled,
                    fingerprint: None,
                    hit: false,
                });
            }
            Err(error) => {
                eprintln!("qk: {}: cache lock unavailable ({error})", task.id);
                return fallback();
            }
        };
        // Inputs may have changed while another worktree held this key's lock.
        if hash::fingerprint(
            workspace,
            graph,
            task,
            prepared,
            &dependencies,
            &self.root,
            cancelled,
        )
        .ok()
        .as_ref()
            != Some(&key)
        {
            return fallback();
        }
        match self.restore(&workspace.root, &key, &outputs) {
            Ok(Some(fingerprint)) => {
                eprintln!("qk: cache hit {}", task.id);
                return Ok(TaskResult {
                    outcome: Outcome::Success,
                    fingerprint: Some(fingerprint),
                    hit: true,
                });
            }
            Ok(None) => {}
            Err(error) => eprintln!("qk: {}: ignoring unusable cache entry ({error})", task.id),
        }
        eprintln!("qk: cache miss {}", task.id);
        let log = match tempfile::NamedTempFile::new_in(self.root.join("tmp")) {
            Ok(log) => log,
            Err(error) => {
                eprintln!("qk: {}: cache log unavailable ({error})", task.id);
                return fallback();
            }
        };
        let capture = Capture::new(log.as_file().try_clone()?, true);
        let outcome = execute_captured(prepared, cancelled, Some(&capture))?;
        let fingerprint = if !self.unchanged(
            workspace,
            graph,
            task,
            prepared,
            &dependencies,
            &key,
            outcome,
            cancelled,
        ) {
            None
        } else if !capture.healthy() {
            output_fingerprint(&workspace.root, &outputs, &key).ok()
        } else {
            // The saved manifest already hashes every output; reuse it.
            match self.publish(&workspace.root, &key, &outputs, log.path()) {
                Ok(fingerprint) => Some(fingerprint),
                Err(error) => {
                    eprintln!("qk: {}: could not save cache entry ({error})", task.id);
                    output_fingerprint(&workspace.root, &outputs, &key).ok()
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
        workspace: &Workspace,
        graph: &TaskGraph,
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
        let after = hash::fingerprint(
            workspace,
            graph,
            task,
            prepared,
            dependencies,
            &self.root,
            cancelled,
        );
        if after.as_deref().ok() != Some(before) {
            if task.definition.cache == Some(true) {
                eprintln!(
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

        let cache = Cache {
            root: root.join(".qk/cache"),
        };
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
