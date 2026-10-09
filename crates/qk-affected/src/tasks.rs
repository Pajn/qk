//! Task-level affectedness: a task is affected when what its cache key reads
//! changed. That is its resolved input files, the lockfile installs of its
//! importers and `externalDependencies`, and `pnpm-workspace.yaml` outside its
//! resolution keys, or it depends on an affected task. Env and runtime inputs
//! count as unchanged, since the base revision's environment is unknowable.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap};

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
    /// A dependency's `package.json` moved its name or version into or out of
    /// what the task's project declares for it, adding or removing that
    /// dependency.
    Relinked { file: String },
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
            Self::Relinked { file } => write!(
                f,
                "{file} moved into or out of what this project declares for it"
            ),
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
        if let Some(file) = relinked.get(&graph.tasks[id].project) {
            reasons.push(TaskReason::Relinked { file: file.clone() });
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
    if tasks.is_empty() {
        return tasks;
    }
    let ids: Vec<&str> = graph.tasks.keys().map(String::as_str).collect();
    let indices: HashMap<&str, usize> = ids
        .iter()
        .enumerate()
        .map(|(index, id)| (*id, index))
        .collect();
    let mut dependents = vec![Vec::new(); ids.len()];
    let mut scheduled = vec![false; ids.len()];
    let mut pending = BinaryHeap::new();
    for (index, id) in ids.iter().enumerate() {
        if tasks.contains_key(*id) || skipped.contains(*id) {
            scheduled[index] = true;
            continue;
        }
        for dependency in &graph.tasks[*id].dependencies {
            if let Some(&dependency_index) = indices.get(dependency.as_str()) {
                dependents[dependency_index].push(index);
            }
            if tasks.contains_key(dependency.as_str()) && !scheduled[index] {
                scheduled[index] = true;
                pending.push(Reverse((0, index)));
            }
        }
    }
    // Reproduce the old sorted scans without visiting unaffected tasks on every
    // pass. A dependency affects a later task in the same pass, or an earlier
    // task in the next one. Ordering those activation events by (pass, index)
    // preserves which dependency supplies the explanation, including diamonds
    // where several dependencies become affected before the task's turn.
    // Each task is queued once and each reverse edge is visited once.
    while let Some(Reverse((pass, index))) = pending.pop() {
        let id = ids[index];
        let dependency = graph.tasks[id]
            .dependencies
            .iter()
            .find(|dependency| tasks.contains_key(dependency.as_str()))
            .expect("a queued task has an affected dependency");
        tasks.insert(
            id.to_owned(),
            TaskCause::DependsOn {
                task: dependency.clone(),
            },
        );
        for &dependent in &dependents[index] {
            if !scheduled[dependent] {
                scheduled[dependent] = true;
                let next_pass = pass + usize::from(dependent < index);
                pending.push(Reverse((next_pass, dependent)));
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use qk_config::Target;
    use qk_taskgraph::Task;

    fn graph(edges: &[(&str, &[&str])]) -> TaskGraph {
        TaskGraph {
            roots: BTreeSet::new(),
            cycles: Vec::new(),
            tasks: edges
                .iter()
                .map(|(id, dependencies)| {
                    (
                        (*id).to_owned(),
                        Task {
                            id: (*id).to_owned(),
                            project: (*id).to_owned(),
                            target: "build".into(),
                            configuration: None,
                            args: Vec::new(),
                            definition: Target::default(),
                            dependencies: dependencies.iter().map(|id| (*id).to_owned()).collect(),
                        },
                    )
                })
                .collect(),
        }
    }

    fn touched(ids: &[&str]) -> BTreeMap<String, Vec<TaskReason>> {
        ids.iter()
            .map(|id| {
                (
                    (*id).to_owned(),
                    vec![TaskReason::Input {
                        file: format!("{id}/source.rs"),
                    }],
                )
            })
            .collect()
    }

    // The previous repeated-scan implementation is retained only as a test
    // oracle for explanation compatibility and the opt-in performance test.
    fn legacy_propagate(
        graph: &TaskGraph,
        touched: &BTreeMap<String, Vec<TaskReason>>,
        skipped: &BTreeSet<String>,
    ) -> BTreeMap<String, TaskCause> {
        let mut tasks: BTreeMap<_, _> = touched
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
        let mut pending: Vec<_> = graph
            .tasks
            .keys()
            .filter(|id| !skipped.contains(*id))
            .collect();
        loop {
            let before = tasks.len();
            pending.retain(|id| {
                if tasks.contains_key(*id) {
                    return false;
                }
                if let Some(dependency) = graph.tasks[*id]
                    .dependencies
                    .iter()
                    .find(|dependency| tasks.contains_key(*dependency))
                {
                    tasks.insert(
                        (*id).clone(),
                        TaskCause::DependsOn {
                            task: dependency.clone(),
                        },
                    );
                    false
                } else {
                    true
                }
            });
            if tasks.len() == before {
                return tasks;
            }
        }
    }

    fn assert_compatible(
        graph: &TaskGraph,
        touched: &BTreeMap<String, Vec<TaskReason>>,
        skipped: &BTreeSet<String>,
    ) {
        assert_eq!(
            serde_json::to_value(propagate(graph, touched, skipped)).unwrap(),
            serde_json::to_value(legacy_propagate(graph, touched, skipped)).unwrap(),
        );
    }

    #[test]
    fn explanations_keep_scan_order_with_multiple_touched_and_skipped_tasks() {
        let graph = graph(&[
            ("a", &["z"]),
            ("b", &[]),
            ("c", &["a", "b"]),
            ("d", &["c"]),
            ("e", &["d"]),
            ("f", &["b", "e"]),
            ("z", &[]),
        ]);
        let touched = touched(&["b", "z"]);
        let skipped = BTreeSet::from(["d".to_owned()]);
        assert_compatible(&graph, &touched, &skipped);
        let tasks = propagate(&graph, &touched, &skipped);
        assert!(matches!(&tasks["c"], TaskCause::DependsOn { task } if task == "a"));
        assert!(!tasks.contains_key("d"));
        assert!(!tasks.contains_key("e"));
        assert!(matches!(&tasks["f"], TaskCause::DependsOn { task } if task == "b"));
        assert!(matches!(&tasks["z"], TaskCause::Touched { reasons }
            if matches!(&reasons[..], [TaskReason::Input { file }] if file == "z/source.rs")));
        // Direct callers historically keep touched entries even if also skipped.
        assert_compatible(&graph, &touched, &BTreeSet::from(["z".to_owned()]));
    }

    #[test]
    fn explanations_match_legacy_scans_across_dependency_shapes() {
        let mut state = 41_u64;
        let mut next = || {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            state >> 32
        };
        let ids: Vec<_> = (0..12).map(|index| format!("task-{index:02}")).collect();
        for _ in 0..256 {
            let mut graph = graph(&[]);
            let mut touched_ids = Vec::new();
            let mut skipped = BTreeSet::new();
            for (index, id) in ids.iter().enumerate() {
                let dependencies: Vec<_> = ids
                    .iter()
                    .enumerate()
                    .filter(|(dependency, _)| *dependency != index && next() % 7 == 0)
                    .map(|(_, id)| id.as_str())
                    .collect();
                graph
                    .tasks
                    .extend(self::graph(&[(id, &dependencies)]).tasks);
                if next() % 5 == 0 {
                    touched_ids.push(id.as_str());
                }
                if next() % 8 == 0 {
                    skipped.insert(id.clone());
                }
            }
            assert_compatible(&graph, &touched(&touched_ids), &skipped);
        }
    }

    fn reverse_chain(count: usize) -> (TaskGraph, BTreeMap<String, Vec<TaskReason>>) {
        let ids: Vec<_> = (0..count).map(|index| format!("task-{index:06}")).collect();
        let mut graph = graph(&[]);
        for (index, id) in ids.iter().enumerate() {
            let dependencies = ids
                .get(index + 1)
                .map(|id| vec![id.as_str()])
                .unwrap_or_default();
            graph
                .tasks
                .extend(self::graph(&[(id, &dependencies)]).tasks);
        }
        let touched = touched(&[ids.last().unwrap()]);
        (graph, touched)
    }

    #[test]
    fn long_reverse_chain_propagates_without_recursion_or_repeated_scans() {
        let (graph, touched) = reverse_chain(20_000);
        let tasks = propagate(&graph, &touched, &BTreeSet::new());
        assert_eq!(tasks.len(), graph.tasks.len());
        assert!(
            matches!(&tasks["task-000000"], TaskCause::DependsOn { task }
            if task == "task-000001")
        );
        assert!(matches!(&tasks["task-019999"], TaskCause::Touched { .. }));
    }

    #[test]
    #[ignore = "manual performance comparison; run with --release --ignored --nocapture"]
    fn benchmark_reverse_chain_propagation() {
        for count in [1_000, 2_000, 4_000, 8_000] {
            let (graph, touched) = reverse_chain(count);
            let skipped = BTreeSet::new();
            let start = std::time::Instant::now();
            let legacy = legacy_propagate(&graph, &touched, &skipped);
            let old_elapsed = start.elapsed();
            let start = std::time::Instant::now();
            let queued = propagate(&graph, &touched, &skipped);
            let new_elapsed = start.elapsed();
            assert_eq!(
                serde_json::to_value(legacy).unwrap(),
                serde_json::to_value(queued).unwrap()
            );
            eprintln!("{count} tasks: repeated scans {old_elapsed:?}, queue {new_elapsed:?}");
        }
    }
}
