//! Task-level affectedness: a task is affected when Nx 24 would select it, or
//! when what its cache key reads changed. Nx matches the change against the
//! task's hash plan: its filesets, the configuration of each project whose
//! inputs it reads, the root tsconfig without other projects' path mappings,
//! and the installed packages it hashes, which a lockfile change moves under
//! `projectsAffectedByDependencyUpdates: "all"`. qk's key adds the lockfile
//! installs of its importers and `externalDependencies`, `pnpm-workspace.yaml`
//! outside its resolution keys and dependency manifests. Nx hashes nx.json
//! whole into every task; qk's key does not, so a change to it reaches a task
//! only as the key holds it: through the settings every key holds, or through
//! the task's definition and resolved inputs, which `targetDefaults` and
//! `namedInputs` decide. A task reading the declared outputs of an affected
//! task is affected too; as in Nx, depending on a task without reading its
//! outputs carries nothing. Env and runtime inputs count as unchanged, since
//! the base revision's environment is unknowable.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap};

use anyhow::Result;
use qk_config::Workspace;
use qk_lockfile::Lockfile;
use qk_taskgraph::{Request, TaskGraph};
use serde::Serialize;

use crate::nx_json::NxJsonChange;
use crate::reachability::{self, Configs, TaskDecision};
use crate::{Changes, FileChange, Options, projections};

/// The affected tasks of a task graph and why each one is affected.
#[derive(Debug, Serialize)]
pub struct TaskAnalysis {
    pub base: Option<String>,
    pub head: Option<String>,
    pub range: Option<crate::Range>,
    pub files: Vec<String>,
    pub tasks: BTreeMap<String, TaskCause>,
    /// Under a profile with reachability, what it decided for each task it
    /// narrows that ordinary selection affects, and for each task affected
    /// only through those it left out.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub reachability: BTreeMap<String, TaskDecision>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "cause", rename_all = "camelCase")]
pub enum TaskCause {
    Touched {
        reasons: Vec<TaskReason>,
    },
    /// Reads the declared outputs of an affected task.
    Reads {
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
    /// A file projects are created from was deleted and qk could not tell
    /// what depended on what it removed, which touches every task.
    DeletedManifest { file: String },
    /// The task's project, or one whose files it reads, depended on a project
    /// the change removed by deleting the manifests it was created from.
    DependedOnRemoved { file: String, project: String },
    /// A file projects are created from changed, in a project whose
    /// configuration the task reads.
    ProjectConfiguration { file: String },
    /// The root tsconfig changed outside `compilerOptions.paths`, or, with
    /// `selectivelyHashTsConfig`, in path mappings into a project the task
    /// reads.
    Tsconfig { file: String },
    /// The lockfile changed, and under this `projectsAffectedByDependencyUpdates`
    /// setting that moves the installed packages the task hashes, or names
    /// its project.
    DependencyUpdates {
        file: String,
        setting: serde_json::Value,
    },
    /// The root `package.json` moved an installed package the task hashes,
    /// or named its project as a dependency.
    RootDependency { name: String, path: String },
    /// The root `package.json` changed in a way that moves every installed
    /// package.
    RootPackage { detail: String },
    /// `nx.json`, the file it extends or `nx.local.json` changed what every
    /// task's key holds, or could not be compared.
    NxJson { file: String, detail: String },
    /// `targetDefaults` or `namedInputs` changed the task's definition.
    Definition { file: String, settings: Vec<String> },
    /// `targetDefaults` or `namedInputs` changed what the task's inputs
    /// select, its own or a dependency's through `^`.
    ResolvedInputs { file: String, settings: Vec<String> },
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
            Self::DependedOnRemoved { file, project } => write!(
                f,
                "it reads a project that depended on {project}, which deleting {file} removes"
            ),
            Self::ProjectConfiguration { file } => {
                write!(
                    f,
                    "{file} changed, which configures a project this task reads"
                )
            }
            Self::Tsconfig { file } => write!(
                f,
                "{file} changed outside compilerOptions.paths, or in path mappings this task reads"
            ),
            Self::DependencyUpdates { file, setting } => write!(
                f,
                "{file} changed, and projectsAffectedByDependencyUpdates is {setting}"
            ),
            Self::RootDependency { name, path } => {
                write!(f, "package.json changed {path}, which moves {name}")
            }
            Self::RootPackage { detail } => write!(f, "package.json {detail}"),
            Self::NxJson { file, detail } => write!(f, "{file} {detail}"),
            Self::Definition { file, settings } => write!(
                f,
                "{file} changed {}, which changes this task's definition",
                settings.join(" and ")
            ),
            Self::ResolvedInputs { file, settings } => write!(
                f,
                "{file} changed {}, which changes what this task's inputs select",
                settings.join(" and ")
            ),
        }
    }
}

pub fn affected_tasks(
    workspace: &Workspace,
    graph: &TaskGraph,
    options: &Options,
) -> Result<TaskAnalysis> {
    let configs = match &options.affected_profile {
        Some(profile) => {
            anyhow::ensure!(
                !projections::has_projections(workspace, profile)?,
                "--affected-profile with projections applies to project selection; task selection uses declared cache inputs"
            );
            projections::reachability(workspace, profile)?
                .map(|reach| Configs::load(workspace, reach))
                .transpose()?
        }
        None => None,
    };
    analyse(
        workspace,
        graph,
        Changes::unfiltered(workspace, options)?,
        configs.as_ref(),
    )
}

/// The tasks of `graph` that `changes` affect, narrowed by `configs` when
/// given.
pub(crate) fn analyse(
    workspace: &Workspace,
    graph: &TaskGraph,
    changes: Changes,
    configs: Option<&Configs>,
) -> Result<TaskAnalysis> {
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
    // Changed files projects are created from, by whether they still exist:
    // a deleted one touches the tasks reading what depended on the project it
    // removed, a changed project.json or package.json the configuration of
    // the project owning it. A changed lockfile configures no project;
    // dependency updates decide what it affects.
    let roots = crate::ProjectRoots::new(workspace);
    let mut deleted = Vec::new();
    let mut reconfigured: BTreeMap<&str, &String> = BTreeMap::new();
    for file in &changes.files {
        if !crate::is_project_config(file) || ignore.matches(file) {
            continue;
        }
        if !workspace.root.join(file).exists() {
            deleted.push(file.clone());
        } else if crate::is_manifest(file)
            && let Some(project) = roots.owner(file)
        {
            reconfigured.entry(project).or_insert(file);
        }
    }
    let removal = crate::removed(workspace, &changes, &deleted);
    let tsconfig = tsconfig_touch(workspace, &changes, &roots);
    let dependency_updates = crate::changed_lockfile(&changes)
        .filter(|file| !ignore.matches(file))
        .map(|file| -> Result<_> {
            let setting = crate::projects_affected_by_dependency_updates(workspace);
            Ok((file, setting, crate::DependencyUpdates::read(workspace)?))
        })
        .transpose()?;
    let npm = crate::root_package_changes(workspace, &changes);
    let resolved = qk_cache::resolve_tasks(workspace, graph, &changes.files)?;
    let nx_json = nx_json_reasons(workspace, graph, &changes, &resolved);
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
        && (changes.whole || {
            let read = |revision: Option<&str>| {
                changes
                    .read("pnpm-workspace.yaml", revision)
                    .and_then(|text| qk_cache::without_resolution(&text))
            };
            match (read(changes.base.as_deref()), read(changes.head.as_deref())) {
                (Some(before), Some(after)) => before != after,
                _ => true,
            }
        });
    // Named files are not compared, so each counts whole.
    let compared = |_: &&String| !changes.whole;
    let mut touched = BTreeMap::new();
    // Changed manifests at both revisions: a dependency's counts for its
    // dependents by what it decides for them.
    let manifests: BTreeMap<String, (Option<String>, Option<String>)> = changes
        .files
        .iter()
        .filter(compared)
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
    .with_manifests(manifests)
    .with_json(
        // Changed files JSON inputs select fields of, at both revisions.
        changes
            .files
            .iter()
            .filter(compared)
            .filter(|file| {
                resolved
                    .values()
                    .any(|inputs| inputs.declared.json_fields.contains_key(*file))
            })
            .map(|file| {
                let before = changes.read(file, changes.base.as_deref());
                let after = changes.read(file, changes.head.as_deref());
                (file.clone(), (before, after))
            })
            .collect(),
    );
    // The head lockfile, to tell which packages a declared external reaches.
    let head_lockfile = changes.lockfile(false).ok();
    for (id, inputs) in &resolved {
        let task = &graph.tasks[id];
        let mut reasons = Vec::new();
        match &removal {
            crate::Removal::None => {}
            crate::Removal::Unknown { file, .. } => {
                reasons.push(TaskReason::DeletedManifest { file: file.clone() });
            }
            crate::Removal::Dependents(dependents) => {
                // At the base, it read the removed project through one of them.
                let read = std::iter::once(&task.project)
                    .chain(&inputs.declared.projects)
                    .find_map(|project| dependents.get(project.as_str()));
                if let Some((file, project)) = read {
                    reasons.push(TaskReason::DependedOnRemoved {
                        file: file.clone(),
                        project: project.clone(),
                    });
                }
            }
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
        // A configuration file the task reads as an input already says so.
        for project in &inputs.declared.projects {
            if let Some(file) = reconfigured.get(project.as_str())
                && !reasons.iter().any(
                    |reason| matches!(reason, TaskReason::Input { file: input } if input == *file),
                )
            {
                reasons.push(TaskReason::ProjectConfiguration {
                    file: (*file).clone(),
                });
            }
        }
        if let Some((file, touch)) = &tsconfig
            && match touch {
                TsconfigTouch::All => true,
                TsconfigTouch::Projects(projects) => {
                    !projects.is_disjoint(&inputs.declared.projects)
                }
            }
        {
            reasons.push(TaskReason::Tsconfig { file: file.clone() });
        }
        // Nx hashes an `@nx/` executor's packages, and every installed
        // package into a task declaring no `externalDependencies`.
        let executor = task.definition.executor.as_deref().unwrap_or_default();
        let nx_executor = executor.starts_with("@nx/") || executor.starts_with("@nrwl/");
        let hashes_externals = nx_executor
            || !inputs.declared.external_declared
            || !inputs.declared.external.is_empty();
        let hashes = |package: &str| {
            nx_executor
                || !inputs.declared.external_declared
                || inputs
                    .declared
                    .external
                    .iter()
                    .any(|name| match head_lockfile {
                        Some(lockfile) => lockfile.reached_packages(name).contains(package),
                        None => name == package,
                    })
        };
        if let Some((file, setting, updates)) = &dependency_updates {
            let touched = match updates {
                crate::DependencyUpdates::All => hashes_externals,
                crate::DependencyUpdates::Projects(projects) => projects.contains(&task.project),
                crate::DependencyUpdates::Auto => false,
            };
            if touched {
                reasons.push(TaskReason::DependencyUpdates {
                    file: (*file).clone(),
                    setting: setting.clone(),
                });
            }
        }
        match &npm {
            Some(crate::NpmChanges::Everything { detail }) if hashes_externals => {
                reasons.push(TaskReason::RootPackage {
                    detail: detail.clone(),
                });
            }
            Some(crate::NpmChanges::Named {
                packages,
                projects,
                all_packages,
            }) => {
                if *all_packages && hashes_externals {
                    reasons.push(TaskReason::RootPackage {
                        detail: "could not be compared".into(),
                    });
                }
                if let Some((name, path)) = packages.iter().find(|(name, _)| hashes(name)) {
                    reasons.push(TaskReason::RootDependency {
                        name: name.clone(),
                        path: path.clone(),
                    });
                }
                if let Some(path) = projects.get(&task.project) {
                    reasons.push(TaskReason::RootDependency {
                        name: task.project.clone(),
                        path: path.clone(),
                    });
                }
            }
            _ => {}
        }
        if let Some(file) = relinked.get(&task.project) {
            reasons.push(TaskReason::Relinked { file: file.clone() });
        }
        match &nx_json {
            NxJsonReasons::None => {}
            NxJsonReasons::All(reason) => reasons.push(reason.clone()),
            NxJsonReasons::Tasks(tasks) => reasons.extend(tasks.get(id).cloned()),
        }
        if !reasons.is_empty() {
            touched.insert(id.clone(), reasons);
        }
    }
    // Each task's producers: the tasks whose declared outputs it reads.
    let producers: Producers = resolved
        .iter()
        .map(|(id, inputs)| (id.clone(), inputs.declared.producers.clone()))
        .collect();
    let mut tasks = propagate(graph, &producers, &touched, &BTreeSet::new());
    let mut decisions = BTreeMap::new();
    if let Some(configs) = configs {
        decisions =
            reachability::decide_tasks(workspace, graph, &producers, &changes, &tasks, configs)?;
        let left_out: BTreeSet<String> = decisions
            .iter()
            .filter(|(_, decision)| matches!(decision, TaskDecision::LeftOut { .. }))
            .map(|(id, _)| id.clone())
            .collect();
        if !left_out.is_empty() {
            touched.retain(|id, _| !left_out.contains(id));
            let narrowed = propagate(graph, &producers, &touched, &left_out);
            for (id, cause) in &tasks {
                if narrowed.contains_key(id) || left_out.contains(id) {
                    continue;
                }
                if let TaskCause::Reads { task } = cause {
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

/// What an nx.json change does to the tasks of a graph.
enum NxJsonReasons {
    None,
    /// Every task, for this reason.
    All(TaskReason),
    /// The tasks whose keys it moves, each with its reason.
    Tasks(BTreeMap<String, TaskReason>),
}

/// The tasks an nx.json change reaches, as their keys hold it: every task
/// when it changed what every key holds, and otherwise each task whose
/// definition or resolved inputs differ under the base revision's
/// `targetDefaults` and `namedInputs`. The base graph is built from the same
/// requests, so that dependents forward the same options; a task only the
/// head graph reaches is requested on its own there.
fn nx_json_reasons(
    workspace: &Workspace,
    graph: &TaskGraph,
    changes: &Changes,
    resolved: &BTreeMap<String, qk_cache::Resolved>,
) -> NxJsonReasons {
    let (file, settings, base) = match NxJsonChange::new(workspace, changes) {
        NxJsonChange::None => return NxJsonReasons::None,
        NxJsonChange::All { file, detail } => {
            return NxJsonReasons::All(TaskReason::NxJson { file, detail });
        }
        NxJsonChange::Resolved {
            file,
            settings,
            base,
        } => (file, settings, base),
    };
    let settings: Vec<String> = settings
        .iter()
        .map(|setting| (*setting).to_owned())
        .collect();
    let compared = (|| -> Result<_> {
        let mut before = graph.rebuild(&base, &[])?;
        let missing: Vec<Request> = graph
            .tasks
            .values()
            .filter(|task| !before.tasks.contains_key(&task.id))
            .map(|task| Request {
                project: task.project.clone(),
                target: task.target.clone(),
                configuration: task.configuration.clone(),
                requested_configuration: task.configuration.clone(),
                args: task.args.clone(),
            })
            .collect();
        if !missing.is_empty() {
            before = graph.rebuild(&base, &missing)?;
        }
        let inputs = qk_cache::resolve_tasks(&base, &before, &changes.files)?;
        Ok((before, inputs))
    })();
    let (before, inputs) = match compared {
        Ok(compared) => compared,
        Err(error) => {
            return NxJsonReasons::All(TaskReason::NxJson {
                file,
                detail: format!(
                    "changed and the base revision's tasks could not be resolved ({error:#})"
                ),
            });
        }
    };
    let definition =
        |task: &qk_taskgraph::Task| serde_json::to_value(qk_cache::keyed_definition(task)).ok();
    let mut tasks = BTreeMap::new();
    for (id, task) in &graph.tasks {
        let reason = match (before.tasks.get(id), inputs.get(id)) {
            (Some(old), Some(old_inputs)) => {
                let inputs = &resolved[id];
                if definition(task) != definition(old) {
                    Some(TaskReason::Definition {
                        file: file.clone(),
                        settings: settings.clone(),
                    })
                } else if !inputs.selects_alike(old_inputs, workspace)
                    // Dependencies' outputs are keyed by the task each came from.
                    || (task.dependencies != old.dependencies
                        && inputs.dependency_output_inputs().next().is_some())
                {
                    Some(TaskReason::ResolvedInputs {
                        file: file.clone(),
                        settings: settings.clone(),
                    })
                } else {
                    None
                }
            }
            _ => Some(TaskReason::Definition {
                file: file.clone(),
                settings: settings.clone(),
            }),
        };
        if let Some(reason) = reason {
            tasks.insert(id.clone(), reason);
        }
    }
    NxJsonReasons::Tasks(tasks)
}

/// Which tasks' `TsConfiguration` a root tsconfig change reaches, as Nx's
/// `ChangedContents` decides it.
enum TsconfigTouch {
    All,
    /// The projects whose own paths entries changed, under
    /// `selectivelyHashTsConfig`; none without it.
    Projects(BTreeSet<String>),
}

/// The root tsconfig that changed and the tasks it reaches, or `None` when
/// neither root tsconfig changed. Nx strips `compilerOptions.paths` from the
/// hash, so a change only there reaches no task, unless
/// `selectivelyHashTsConfig` keys each project's own entries. A change
/// elsewhere, one it cannot compare, or a change to the root tsconfig it does
/// not read, which may have switched which file is the root, reaches all.
fn tsconfig_touch(
    workspace: &Workspace,
    changes: &Changes,
    roots: &crate::ProjectRoots,
) -> Option<(String, TsconfigTouch)> {
    let file = changes
        .files
        .iter()
        .find(|file| qk_cache::ROOT_TSCONFIG_FILES.contains(&file.as_str()))?;
    let root = qk_cache::ROOT_TSCONFIG_FILES
        .into_iter()
        .find(|name| workspace.root.join(name).exists())
        .unwrap_or(qk_cache::ROOT_TSCONFIG_FILES[1]);
    let switched = changes
        .files
        .iter()
        .any(|file| qk_cache::ROOT_TSCONFIG_FILES.contains(&file.as_str()) && file != root);
    let read = |revision: Option<&str>| {
        changes
            .read(root, revision)
            .and_then(|text| qk_cache::TsConfig::parse(&text))
    };
    let compared = if switched || changes.whole {
        None
    } else {
        read(changes.base.as_deref()).zip(read(changes.head.as_deref()))
    };
    let touch = match compared {
        Some((before, after)) if before.rest == after.rest => {
            let mut projects = BTreeSet::new();
            if qk_cache::selectively_hash_tsconfig(workspace) {
                let keys: BTreeSet<&String> =
                    before.paths.keys().chain(after.paths.keys()).collect();
                for key in keys {
                    // Each owner's targets in their order, as the hash keeps them.
                    let owned = |paths: &BTreeMap<String, Vec<String>>| {
                        let mut owned: BTreeMap<&str, Vec<String>> = BTreeMap::new();
                        for target in paths.get(key).into_iter().flatten() {
                            if let Some(owner) = roots.owner(target) {
                                owned.entry(owner).or_default().push(target.clone());
                            }
                        }
                        owned
                    };
                    let (old, new) = (owned(&before.paths), owned(&after.paths));
                    for owner in old.keys().chain(new.keys()) {
                        if old.get(owner) != new.get(owner) {
                            projects.insert((*owner).to_owned());
                        }
                    }
                }
            }
            TsconfigTouch::Projects(projects)
        }
        _ => TsconfigTouch::All,
    };
    Some((file.clone(), touch))
}

/// Each task's producers: the tasks whose declared outputs it reads.
pub(crate) type Producers = BTreeMap<String, BTreeSet<String>>;

/// The touched tasks and, in dependency order, every task reading the outputs
/// of an affected one, except `skipped`.
fn propagate(
    graph: &TaskGraph,
    producers: &Producers,
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
        for dependency in producers.get(*id).into_iter().flatten() {
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
        let dependency = producers[id]
            .iter()
            .find(|dependency| tasks.contains_key(dependency.as_str()))
            .expect("a queued task has an affected dependency");
        tasks.insert(
            id.to_owned(),
            TaskCause::Reads {
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
                            edges: BTreeMap::new(),
                        },
                    )
                })
                .collect(),
            ..TaskGraph::default()
        }
    }

    /// Producers for these graphs: each task reads its dependencies' outputs.
    fn reads(graph: &TaskGraph) -> Producers {
        graph
            .tasks
            .iter()
            .map(|(id, task)| (id.clone(), task.dependencies.clone()))
            .collect()
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
                if let Some(dependency) = reads(graph)[*id]
                    .iter()
                    .find(|dependency| tasks.contains_key(*dependency))
                {
                    tasks.insert(
                        (*id).clone(),
                        TaskCause::Reads {
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
            serde_json::to_value(propagate(graph, &reads(graph), touched, skipped)).unwrap(),
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
        let tasks = propagate(&graph, &reads(&graph), &touched, &skipped);
        assert!(matches!(&tasks["c"], TaskCause::Reads { task } if task == "a"));
        assert!(!tasks.contains_key("d"));
        assert!(!tasks.contains_key("e"));
        assert!(matches!(&tasks["f"], TaskCause::Reads { task } if task == "b"));
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
        let tasks = propagate(&graph, &reads(&graph), &touched, &BTreeSet::new());
        assert_eq!(tasks.len(), graph.tasks.len());
        assert!(matches!(&tasks["task-000000"], TaskCause::Reads { task }
            if task == "task-000001"));
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
            let queued = propagate(&graph, &reads(&graph), &touched, &skipped);
            let new_elapsed = start.elapsed();
            assert_eq!(
                serde_json::to_value(legacy).unwrap(),
                serde_json::to_value(queued).unwrap()
            );
            eprintln!("{count} tasks: repeated scans {old_elapsed:?}, queue {new_elapsed:?}");
        }
    }
}
