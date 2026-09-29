//! Task-level affectedness: a task is affected when what its cache key reads
//! changed. That is its resolved input files, the lockfile installs of its
//! importers and `externalDependencies`, and `pnpm-workspace.yaml` outside its
//! resolution keys, or it depends on an affected task. Env and runtime inputs
//! count as unchanged, since the base revision's environment is unknowable.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use qk_config::Workspace;
use qk_lockfile::Lockfile;
use qk_taskgraph::TaskGraph;
use serde::Serialize;

use crate::{Changes, FileChange, Options};

/// The affected tasks of a task graph and why each one is affected.
#[derive(Debug, Serialize)]
pub struct TaskAnalysis {
    pub base: Option<String>,
    pub head: Option<String>,
    pub files: Vec<String>,
    pub tasks: BTreeMap<String, TaskCause>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "cause", rename_all = "camelCase")]
pub enum TaskCause {
    Touched {
        reasons: Vec<TaskReason>,
    },
    /// Depends on an affected task.
    DependsOn {
        task: String,
    },
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "reason", rename_all = "camelCase")]
pub enum TaskReason {
    /// A changed file is one of the task's inputs.
    Input { file: String },
    /// What an importer the task uses installs differs.
    Installs { importer: String },
    /// A package named by `externalDependencies` is installed differently.
    Package { name: String },
    /// The lockfile's version, settings or package manager changed.
    Lockfile,
    /// `pnpm-workspace.yaml` changed outside its resolution keys.
    WorkspaceFile,
    /// A project manifest was deleted, which touches every task.
    DeletedManifest { file: String },
}

impl std::fmt::Display for TaskReason {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::Input { file } => write!(f, "input {file} changed"),
            Self::Installs { importer } => write!(f, "what {importer} installs changed"),
            Self::Package { name } => write!(f, "external dependency {name} changed"),
            Self::Lockfile => write!(f, "the lockfile's version or settings changed"),
            Self::WorkspaceFile => {
                write!(f, "pnpm-workspace.yaml changed outside its resolution keys")
            }
            Self::DeletedManifest { file } => write!(f, "{file} was deleted"),
        }
    }
}

pub fn affected_tasks(
    workspace: &Workspace,
    graph: &TaskGraph,
    options: &Options,
) -> Result<TaskAnalysis> {
    let changes = Changes::new(workspace, options)?;
    let changed: BTreeSet<&str> = changes.files.iter().map(String::as_str).collect();
    let deleted_manifest = changes.files.iter().find(|file| {
        let name = file.rsplit('/').next().unwrap_or(file);
        matches!(name, "project.json" | "package.json") && !workspace.root.join(file).exists()
    });
    let resolved = qk_cache::resolve_tasks(workspace, graph, &changes.files)?;
    // The lockfile at both revisions, when it changed and both can be read.
    let lockfiles = if changed.contains("pnpm-lock.yaml") {
        match changes.change("pnpm-lock.yaml") {
            FileChange::Lockfile { before, after } => Lockfile::parse(&before)
                .ok()
                .zip(Lockfile::parse(&after).ok()),
            _ => None,
        }
    } else {
        None
    };
    let workspace_file_changed = changed.contains("pnpm-workspace.yaml") && {
        let read = |revision: Option<&str>| {
            changes
                .read("pnpm-workspace.yaml", revision)
                .and_then(|text| qk_cache::without_resolution(&text))
        };
        match (read(changes.base.as_deref()), read(changes.head.as_deref())) {
            (Some(before), Some(after)) => before != after,
            _ => true,
        }
    };
    let mut tasks = BTreeMap::new();
    for (id, inputs) in &resolved {
        let mut reasons = Vec::new();
        if let Some(file) = deleted_manifest {
            reasons.push(TaskReason::DeletedManifest { file: file.clone() });
        }
        for file in inputs
            .files
            .iter()
            .filter(|file| changed.contains(file.as_str()))
        {
            reasons.push(TaskReason::Input { file: file.clone() });
        }
        if inputs.workspace_file && workspace_file_changed {
            reasons.push(TaskReason::WorkspaceFile);
        }
        match (&inputs.lockfile, &lockfiles) {
            (Some((importers, external)), Some((before, after))) => {
                if before.global() != after.global() {
                    reasons.push(TaskReason::Lockfile);
                }
                for importer in importers {
                    if before.installed(importer) != after.installed(importer) {
                        reasons.push(TaskReason::Installs {
                            importer: importer.clone(),
                        });
                    }
                }
                for name in external {
                    if before.package(name) != after.package(name) {
                        reasons.push(TaskReason::Package { name: name.clone() });
                    }
                }
            }
            // The lockfile changed but qk could not compare it.
            (Some(_), None) if changed.contains("pnpm-lock.yaml") => {
                reasons.push(TaskReason::Input {
                    file: "pnpm-lock.yaml".into(),
                })
            }
            _ => {}
        }
        if !reasons.is_empty() {
            tasks.insert(id.clone(), TaskCause::Touched { reasons });
        }
    }
    // Dependents of an affected task are affected, in dependency order.
    let mut pending: Vec<&str> = graph.tasks.keys().map(String::as_str).collect();
    loop {
        let before = tasks.len();
        pending.retain(|id| {
            if tasks.contains_key(*id) {
                return false;
            }
            match graph.tasks[*id]
                .dependencies
                .iter()
                .find(|dependency| tasks.contains_key(dependency.as_str()))
            {
                Some(dependency) => {
                    tasks.insert(
                        (*id).to_owned(),
                        TaskCause::DependsOn {
                            task: dependency.clone(),
                        },
                    );
                    false
                }
                None => true,
            }
        });
        if tasks.len() == before {
            break;
        }
    }
    Ok(TaskAnalysis {
        base: changes.base,
        head: changes.head,
        files: changes.files,
        tasks,
    })
}
