//! Local content-addressed cache shared by linked Git worktrees.

mod hash;
mod paths;
mod store;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use qk_config::Workspace;
use qk_executor::{Capture, Outcome, PreparedTask, execute, execute_captured};
use qk_taskgraph::{Task, TaskGraph};

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
            return Ok(TaskResult {
                outcome,
                fingerprint: self.after(
                    workspace,
                    graph,
                    task,
                    prepared,
                    &dependencies,
                    &key,
                    &outputs,
                    outcome,
                    cancelled,
                ),
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
            Ok(true) => {
                eprintln!("qk: cache hit {}", task.id);
                return Ok(TaskResult {
                    outcome: Outcome::Success,
                    fingerprint: output_fingerprint(&workspace.root, &outputs, &key).ok(),
                    hit: true,
                });
            }
            Ok(false) => {}
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
        let fingerprint = self.after(
            workspace,
            graph,
            task,
            prepared,
            &dependencies,
            &key,
            &outputs,
            outcome,
            cancelled,
        );
        if fingerprint.is_some()
            && capture.healthy()
            && let Err(error) = self.publish(&workspace.root, &key, &outputs, log.path())
        {
            eprintln!("qk: {}: could not save cache entry ({error})", task.id);
        }
        Ok(TaskResult {
            outcome,
            fingerprint,
            hit: false,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn after(
        &self,
        workspace: &Workspace,
        graph: &TaskGraph,
        task: &Task,
        prepared: &PreparedTask,
        dependencies: &BTreeMap<String, String>,
        before: &str,
        outputs: &paths::Outputs,
        outcome: Outcome,
        cancelled: &AtomicBool,
    ) -> Option<String> {
        if outcome != Outcome::Success || cancelled.load(Ordering::SeqCst) {
            return None;
        }
        let after = hash::fingerprint(
            workspace,
            graph,
            task,
            prepared,
            dependencies,
            &self.root,
            cancelled,
        )
        .ok()?;
        if after != before {
            if task.definition.cache == Some(true) {
                eprintln!(
                    "qk: {}: not caching because inputs changed during execution",
                    task.id
                );
            }
            return None;
        }
        output_fingerprint(&workspace.root, outputs, before).ok()
    }
}

fn output_fingerprint(
    root: &std::path::Path,
    outputs: &paths::Outputs,
    input: &str,
) -> Result<String> {
    let mut files = BTreeMap::new();
    for path in outputs.paths(root)? {
        let absolute = root.join(&path);
        let metadata = std::fs::symlink_metadata(&absolute)?;
        if metadata.file_type().is_symlink() {
            files.insert(
                path,
                serde_json::json!({"link": std::fs::read_link(absolute)?}),
            );
        } else if metadata.is_file() {
            files.insert(path.clone(), hash::file_value(root, &path)?);
        } else {
            files.insert(path, serde_json::json!("directory"));
        }
    }
    Ok(blake3::hash(&serde_json::to_vec(&(input, files))?)
        .to_hex()
        .to_string())
}
