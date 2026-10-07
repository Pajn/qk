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

use crate::reachability::{self, TaskDecision};
use crate::{Changes, FileChange, Options, projections};

/// The affected tasks of a task graph and why each one is affected.
#[derive(Debug, Serialize)]
pub struct TaskAnalysis {
    pub base: Option<String>,
    pub head: Option<String>,
    pub range: Option<crate::Range>,
    pub files: Vec<String>,
    pub tasks: BTreeMap<String, TaskCause>,
    /// Under a profile with reachability, what it decided for each task with
    /// `qk:reachability` that ordinary selection affects, and for each task
    /// affected only through those it left out.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub reachability: BTreeMap<String, TaskDecision>,
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
    let reach = match &options.affected_profile {
        Some(profile) => {
            anyhow::ensure!(
                !projections::has_projections(workspace, profile)?,
                "--affected-profile with projections applies to project selection; task selection uses declared cache inputs"
            );
            true
        }
        None => false,
    };
    let changes = Changes::unfiltered(workspace, options)?;
    if changes.files.is_empty() {
        return Ok(TaskAnalysis {
            base: changes.base,
            head: changes.head,
            range: changes.range,
            files: changes.files,
            tasks: BTreeMap::new(),
            reachability: BTreeMap::new(),
        });
    }
    let ignore = qk_cache::SourceIgnore::new(&workspace.root)?;
    let deleted_manifest = changes.files.iter().find(|file| {
        let name = file.rsplit('/').next().unwrap_or(file);
        matches!(name, "project.json" | "package.json")
            && !ignore.matches(file)
            && !workspace.root.join(file).exists()
    });
    let resolved = qk_cache::resolve_tasks(workspace, graph, &changes.files)?;
    // The lockfile at both revisions, when it changed and both can be read.
    let lockfiles = if changes.files.iter().any(|file| file == "pnpm-lock.yaml") {
        match changes.change("pnpm-lock.yaml") {
            FileChange::Lockfile { before, after } => Lockfile::parse(&before)
                .ok()
                .zip(Lockfile::parse(&after).ok()),
            _ => None,
        }
    } else {
        None
    };
    let workspace_file_changed = changes
        .files
        .iter()
        .any(|file| file == "pnpm-workspace.yaml")
        && {
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
    let mut touched = BTreeMap::new();
    // Changed manifests at both revisions: a dependency's counts for its
    // dependents by what it decides for them.
    let manifests: BTreeMap<String, (Option<String>, Option<String>)> = changes
        .files
        .iter()
        .filter(|file| file.rsplit('/').next() == Some("package.json"))
        .map(|file| {
            let before = changes.read(file, changes.base.as_deref());
            let after = changes.read(file, changes.head.as_deref());
            (file.clone(), (before, after))
        })
        .collect();
    let relinked = relinked_dependents(workspace, &manifests);
    let mut input_changes = qk_cache::InputChanges::new(
        &workspace.root,
        &changes.files,
        lockfiles.as_ref().map(|(before, after)| (before, after)),
        workspace_file_changed,
    )?
    .with_manifests(manifests);
    for (id, inputs) in &resolved {
        let mut reasons = Vec::new();
        if let Some(file) = deleted_manifest {
            reasons.push(TaskReason::DeletedManifest { file: file.clone() });
        }
        reasons.extend(
            input_changes
                .reasons(inputs)?
                .into_iter()
                .map(|change| match change {
                    qk_cache::InputChange::Input { file } => TaskReason::Input { file },
                    qk_cache::InputChange::Installs { importer } => {
                        TaskReason::Installs { importer }
                    }
                    qk_cache::InputChange::Package { name } => TaskReason::Package { name },
                    qk_cache::InputChange::Lockfile => TaskReason::Lockfile,
                    qk_cache::InputChange::WorkspaceFile => TaskReason::WorkspaceFile,
                }),
        );
        if let Some(file) = relinked.get(&graph.tasks[id].project)
            && !reasons
                .iter()
                .any(|reason| matches!(reason, TaskReason::Input { file: input } if input == file))
        {
            reasons.push(TaskReason::Input { file: file.clone() });
        }
        if !reasons.is_empty() {
            touched.insert(id.clone(), reasons);
        }
    }
    let mut tasks = propagate(graph, &touched, &BTreeSet::new());
    let mut decisions = BTreeMap::new();
    if reach {
        decisions = reachability::decide_tasks(workspace, graph, &changes, &tasks)?;
        let left_out: BTreeSet<String> = decisions
            .iter()
            .filter(|(_, decision)| matches!(decision, TaskDecision::LeftOut { .. }))
            .map(|(id, _)| id.clone())
            .collect();
        if !left_out.is_empty() {
            touched.retain(|id, _| !left_out.contains(id));
            let narrowed = propagate(graph, &touched, &left_out);
            for (id, cause) in &tasks {
                if narrowed.contains_key(id) || left_out.contains(id) {
                    continue;
                }
                if let TaskCause::DependsOn { task } = cause {
                    decisions.insert(id.clone(), TaskDecision::Through { task: task.clone() });
                }
            }
            tasks = narrowed;
        }
    }
    Ok(TaskAnalysis {
        base: changes.base,
        head: changes.head,
        range: changes.range,
        files: changes.files,
        tasks,
        reachability: decisions,
    })
}

/// The touched tasks and, in dependency order, every task depending on an
/// affected one, except `skipped`.
fn propagate(
    graph: &TaskGraph,
    touched: &BTreeMap<String, Vec<TaskReason>>,
    skipped: &BTreeSet<String>,
) -> BTreeMap<String, TaskCause> {
    let mut tasks: BTreeMap<String, TaskCause> = touched
        .iter()
        .map(|(id, reasons)| {
            (
                id.clone(),
                TaskCause::Touched {
                    reasons: reasons.clone(),
                },
            )
        })
        .collect();
    let mut pending: Vec<&str> = graph
        .tasks
        .keys()
        .map(String::as_str)
        .filter(|id| !skipped.contains(*id))
        .collect();
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
    tasks
}

/// Projects whose dependency on a workspace package was added or removed
/// because the package's name or version changed, so that what they declare
/// for it started or stopped meaning it, each with that package's manifest.
/// The keys of all their tasks change with the dependency, although the
/// version itself is not keyed for them.
fn relinked_dependents(
    workspace: &Workspace,
    manifests: &BTreeMap<String, (Option<String>, Option<String>)>,
) -> BTreeMap<String, String> {
    let identity = |text: &Option<String>| {
        let value: serde_json::Value = serde_json::from_str(text.as_deref()?).ok()?;
        let name = value.get("name")?.as_str()?.to_owned();
        let version = value
            .get("version")
            .and_then(|version| version.as_str())
            .map(String::from);
        Some((name, version))
    };
    // Whether a dependency declared as `name` with `range` means this package,
    // as the project graph decides it: by name, then by a range that links
    // whatever the version or one the version satisfies.
    let links = |package: &Option<(String, Option<String>)>, name: &str, range: &str| {
        package.as_ref().is_some_and(|(own, version)| {
            own == name
                && (range.starts_with("workspace:")
                    || range == "*"
                    || range.starts_with("file:")
                    || version
                        .as_deref()
                        .is_some_and(|version| qk_graph::satisfies(version, range)))
        })
    };
    let mut relinked = BTreeMap::new();
    for (file, (before, after)) in manifests {
        let (before, after) = (identity(before), identity(after));
        if before == after {
            continue;
        }
        // The name at both revisions, once when it is unchanged.
        let names: BTreeSet<&String> = before
            .iter()
            .chain(after.iter())
            .map(|(name, _)| name)
            .collect();
        for name in names {
            for (project, package) in &workspace.packages {
                let Some(range) = package.dependency_ranges().get(name.as_str()).copied() else {
                    continue;
                };
                if links(&before, name, range) != links(&after, name, range) {
                    relinked.insert(project.clone(), file.clone());
                }
            }
        }
    }
    relinked
}
