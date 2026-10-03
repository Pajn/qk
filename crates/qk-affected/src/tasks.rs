//! Task-level affectedness: a task is affected when what its cache key reads
//! changed. That is its resolved input files, the lockfile installs of its
//! importers and `externalDependencies`, and `pnpm-workspace.yaml` outside its
//! resolution keys, or it depends on an affected task. Env and runtime inputs
//! count as unchanged, since the base revision's environment is unknowable.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

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
    pub range: Option<crate::Range>,
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
    let changes = Changes::unfiltered(workspace, options)?;
    if changes.files.is_empty() {
        return Ok(TaskAnalysis {
            base: changes.base,
            head: changes.head,
            range: changes.range,
            files: changes.files,
            tasks: BTreeMap::new(),
        });
    }
    let ignore = qk_cache::SourceIgnore::new(&workspace.root)?;
    let changed: BTreeSet<&str> = changes.files.iter().map(String::as_str).collect();
    let deleted_manifest = changes.files.iter().find(|file| {
        let name = file.rsplit('/').next().unwrap_or(file);
        matches!(name, "project.json" | "package.json")
            && !ignore.matches(file)
            && !workspace.root.join(file).exists()
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
    let canonical_root = workspace.root.canonicalize()?;
    let mut links = BTreeMap::new();
    let mut directory_links = BTreeMap::new();
    for (id, inputs) in &resolved {
        let mut reasons = Vec::new();
        if let Some(file) = deleted_manifest {
            reasons.push(TaskReason::DeletedManifest { file: file.clone() });
        }
        let mut touched = BTreeSet::new();
        for file in inputs.files.iter().map(String::as_str).chain(
            inputs
                .values
                .keys()
                .filter_map(|key| key.strip_prefix("json:")),
        ) {
            if changed.contains(file) {
                touched.insert(file);
                continue;
            }
            if !links.contains_key(file) {
                links.insert(
                    file.to_owned(),
                    symlink_targets(&canonical_root, file, &mut directory_links)?,
                );
            }
            for target in &links[file] {
                touched.extend(
                    changes
                        .files
                        .iter()
                        .filter(|changed| {
                            target.is_empty()
                                || **changed == *target
                                || changed
                                    .strip_prefix(target)
                                    .is_some_and(|suffix| suffix.starts_with('/'))
                        })
                        .map(String::as_str),
                );
            }
        }
        for file in touched {
            reasons.push(TaskReason::Input {
                file: file.to_owned(),
            });
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
        range: changes.range,
        files: changes.files,
        tasks,
    })
}

/// Paths a selected symlink reads, including a target that has been deleted.
fn symlink_targets(
    root: &Path,
    path: &str,
    directory_links: &mut BTreeMap<PathBuf, Vec<PathBuf>>,
) -> Result<Vec<String>> {
    let mut pending = vec![root.join(path)];
    let mut visited = BTreeSet::new();
    let mut targets = BTreeSet::new();
    while let Some(absolute) = pending.pop() {
        if !visited.insert(absolute.clone()) {
            continue;
        }
        let metadata = match std::fs::symlink_metadata(&absolute) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if !metadata.file_type().is_symlink() {
            continue;
        }
        let target = absolute
            .parent()
            .expect("input has a parent")
            .join(std::fs::read_link(&absolute)?);
        let mut normalized = PathBuf::new();
        for component in target.components() {
            match component {
                Component::CurDir => {}
                Component::ParentDir => {
                    normalized.pop();
                }
                component => normalized.push(component.as_os_str()),
            }
        }
        let resolved = target.canonicalize().ok().or_else(|| {
            Some(
                target
                    .parent()?
                    .canonicalize()
                    .ok()?
                    .join(target.file_name()?),
            )
        });
        if normalized.starts_with(root) {
            pending.push(normalized.clone());
        }
        if let Some(resolved) = &resolved
            && resolved.starts_with(root)
            && resolved.is_dir()
        {
            if !directory_links.contains_key(resolved) {
                let mut links = Vec::new();
                for entry in walkdir::WalkDir::new(resolved).follow_links(false) {
                    let entry = entry?;
                    if entry.file_type().is_symlink() {
                        links.push(entry.into_path());
                    }
                }
                directory_links.insert(resolved.clone(), links);
            }
            pending.extend(directory_links[resolved].iter().cloned());
        }
        for target in std::iter::once(normalized).chain(resolved) {
            if let Ok(relative) = target.strip_prefix(root)
                && let Some(relative) = relative.to_str()
            {
                targets.insert(relative.replace('\\', "/"));
            }
        }
    }
    Ok(targets.into_iter().collect())
}
