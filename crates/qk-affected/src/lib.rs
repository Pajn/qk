//! Which projects a set of changed files affects, following Nx's
//! `filterAffected`: files are attributed to projects by the locators below,
//! then every project depending on a touched one is affected too.
//!
//! One locator is deliberately more precise than Nx. For a `pnpm-lock.yaml`
//! change under `projectsAffectedByDependencyUpdates: "auto"`, a project is
//! touched when what its importer installs differs between the revisions.
//! Nx diffs versions by package name, so it also reports projects that install
//! exactly what they did before. A change to nx.json or to a project's
//! `project.json` likewise touches only what it changes, where Nx touches
//! every project or every project reading the file; see `nx_json` and
//! `manifests`.

mod json_diff;
mod manifests;
mod nx_json;
mod projections;
mod reachability;
mod subprocess;
mod tasks;

pub use projections::{ProjectionFallbackKind, ProjectionFallbackPolicy, ProjectionReport};
pub use reachability::{Decision, Kept, TaskDecision};
pub use tasks::{TaskAnalysis, TaskCause, TaskReason, affected_tasks};

use std::cell::OnceCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use subprocess::workspace_command;

use anyhow::{Context, Result, bail, ensure};
use qk_cache::Pattern;
use qk_config::Workspace;
use qk_graph::ProjectGraph;
use qk_lockfile::{Installation, Lockfile};
use serde::Serialize;
use serde_json::Value;

use json_diff::{Change, Kind};

/// What to compare, as Nx's affected options spell it.
#[derive(Clone, Debug, Default)]
pub struct Options {
    pub affected_profile: Option<String>,
    pub fail_on_projection_fallback: Option<ProjectionFallbackPolicy>,
    pub base: Option<String>,
    pub head: Option<String>,
    pub files: Vec<String>,
    /// An explicitly supplied list can be empty, such as empty stdin.
    pub explicit_files: bool,
    pub uncommitted: bool,
    pub untracked: bool,
}

/// The lockfiles Nx reads at both revisions rather than diffing as JSON or
/// YAML, by name; at the workspace root, a change to one is a dependency
/// update (Nx's `AUTO_AFFECTED_LOCK_FILES`).
const LOCKFILES: &[&str] = &[
    "pnpm-lock.yaml",
    "pnpm-lock.yml",
    "package-lock.json",
    "yarn.lock",
    "bun.lock",
    "bun.lockb",
];

/// The root lockfiles the js plugin's `createNodes` glob picks up, which a
/// deletion therefore treats as project configuration.
const PLUGIN_LOCKFILES: &[&str] = &[
    "yarn.lock",
    "package-lock.json",
    "pnpm-lock.yaml",
    "bun.lockb",
    "bun.lock",
];

/// Whether `file` is what Nx's default plugins create projects from: any
/// `project.json` or `package.json` by their `createNodes` globs, whether or
/// not package discovery then applies to it, or a root lockfile.
pub(crate) fn is_project_config(file: &str) -> bool {
    is_manifest(file) || PLUGIN_LOCKFILES.contains(&file)
}

/// Whether `file` is a `project.json` or `project.local.json`, which counts
/// by what it changes.
pub(crate) fn is_project_file(file: &str) -> bool {
    matches!(
        file.rsplit('/').next().unwrap_or(file),
        "project.json" | qk_config::LOCAL_OVERRIDES
    )
}

/// Whether `file` is a `project.json` or `package.json`.
pub(crate) fn is_manifest(file: &str) -> bool {
    matches!(
        file.rsplit('/').next().unwrap_or(file),
        "project.json" | "package.json"
    )
}

pub fn affected_projects(
    workspace: &Workspace,
    graph: &ProjectGraph,
    options: &Options,
) -> Result<BTreeSet<String>> {
    Ok(analyse(workspace, graph, options)?
        .projects
        .into_keys()
        .collect())
}

/// The affected projects and why each one is affected.
#[derive(Debug, Serialize)]
pub struct Analysis {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub projections: Vec<ProjectionReport>,
    /// Changed paths after ignore rules, before applying a selected profile.
    #[serde(rename = "originalFiles", skip_serializing_if = "Option::is_none")]
    pub original_files: Option<Vec<String>>,
    /// The merge base compared against.
    pub base: Option<String>,
    /// The head revision; `None` compares the working tree.
    pub head: Option<String>,
    /// How the base was found, when changes come from a commit range.
    pub range: Option<Range>,
    /// The changed files considered, after ignore rules.
    pub files: Vec<String>,
    pub projects: BTreeMap<String, Cause>,
    /// Under a profile with reachability, what it decided for each affected
    /// project with a task it narrows.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub reachability: BTreeMap<String, Decision>,
    /// Why each project the profile left out was affected, so a project
    /// affected through one still explains its chain.
    #[serde(skip)]
    left_out: BTreeMap<String, Cause>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "cause", rename_all = "camelCase")]
pub enum Cause {
    /// The project itself is touched by a change.
    Touched { reasons: Vec<Reason> },
    /// The project depends on an affected project. Of the paths to a touched
    /// project, this is the first step of a shortest one.
    DependsOn {
        project: String,
        kind: String,
        /// The project's other affected direct dependencies.
        also: Vec<String>,
    },
}

/// Why a locator touches a project.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "reason", rename_all = "camelCase")]
pub enum Reason {
    /// A changed file lies under the project's root.
    File { file: String },
    /// `nx.json`, the file it extends or `nx.local.json` changed what every
    /// task's key holds, or could not be compared, which touches every
    /// project.
    NxJson { file: String, detail: String },
    /// `targetDefaults` or `namedInputs` changed, and with them these targets
    /// and named inputs of the project.
    NxJsonConfiguration {
        file: String,
        settings: Vec<String>,
        targets: Vec<String>,
        #[serde(rename = "namedInputs")]
        named_inputs: Vec<String>,
    },
    /// A `project.json` or `project.local.json` changed the project's
    /// configuration: these targets and named inputs, these other keys, or
    /// what its dependencies or inputs naming projects select.
    ProjectConfiguration {
        files: Vec<String>,
        targets: Vec<String>,
        #[serde(rename = "namedInputs")]
        named_inputs: Vec<String>,
        fields: Vec<String>,
    },
    /// A changed file matches a `{workspaceRoot}` input of one of its targets.
    WorkspaceInput {
        file: String,
        target: String,
        input: String,
    },
    /// A file projects are created from was deleted and qk could not tell
    /// what depended on what it removed, which touches every project.
    DeletedManifest { file: String, detail: String },
    /// The project depended on a project or package the change removed, by
    /// deleting the manifest it was created from: `project` names it.
    DependedOnRemoved { file: String, project: String },
    /// The lockfile changed under a `projectsAffectedByDependencyUpdates`
    /// setting that does not look inside it.
    DependencyUpdates { file: String, setting: Value },
    /// The lockfile changed and qk could not compare its revisions, which
    /// touches every project.
    UnreadableLockfile { file: String, detail: String },
    /// What the project's importer installs differs between the revisions.
    Installs {
        file: String,
        importer: String,
        /// Direct dependencies whose resolution changed, as `name: before -> after`.
        direct: Vec<String>,
        /// Snapshot keys installed only after the change.
        added: Vec<String>,
        /// Snapshot keys installed only before the change.
        removed: Vec<String>,
        /// Snapshot keys whose integrity or dependencies changed.
        changed: Vec<String>,
    },
    /// A dependency or override in the root `package.json` changed, and the
    /// project installs that package or is it.
    RootDependency { name: String, path: String },
    /// A root `package.json` change that touches every project.
    RootPackage { detail: String },
    /// A path mapping in the root tsconfig points into the project.
    TsconfigPath {
        file: String,
        mapping: String,
        path: String,
    },
    /// A root tsconfig change outside `compilerOptions.paths`, which touches
    /// every project.
    Tsconfig { file: String },
}

impl std::fmt::Display for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::File { file } => write!(f, "{file} changed"),
            Self::NxJson { file, detail } => {
                write!(f, "{file} {detail}, which affects every project")
            }
            Self::NxJsonConfiguration {
                file,
                settings,
                targets,
                named_inputs,
            } => write!(
                f,
                "{file} changed {}, which changes its {}",
                settings.join(" and "),
                changed_parts(targets, named_inputs, &[])
            ),
            Self::ProjectConfiguration {
                files,
                targets,
                named_inputs,
                fields,
            } => write!(
                f,
                "{} changed, which changes its {}",
                files.join(", "),
                changed_parts(targets, named_inputs, fields)
            ),
            Self::WorkspaceInput {
                file,
                target,
                input,
            } => write!(
                f,
                "{file} changed, matching input {input} of target {target}"
            ),
            Self::DeletedManifest { file, detail } => write!(
                f,
                "{file} was deleted, which affects every project because {detail}"
            ),
            Self::DependedOnRemoved { file, project } => {
                write!(f, "it depended on {project}, which deleting {file} removes")
            }
            Self::DependencyUpdates { file, setting } => write!(
                f,
                "{file} changed, and projectsAffectedByDependencyUpdates is {setting}"
            ),
            Self::UnreadableLockfile { file, detail } => write!(
                f,
                "{file} changed and could not be compared ({detail}), which affects every project"
            ),
            Self::Installs {
                file,
                importer,
                added,
                removed,
                changed,
                ..
            } => {
                let packages: BTreeSet<&str> = added
                    .iter()
                    .chain(removed)
                    .chain(changed)
                    .map(|key| package_name(key))
                    .collect();
                let count = packages.len();
                let noun = if count == 1 { "package" } else { "packages" };
                write!(
                    f,
                    "what {importer} installs changed in {file} ({count} {noun})"
                )
            }
            Self::RootDependency { name, path } => {
                write!(
                    f,
                    "package.json changed {path}, and this project installs {name}"
                )
            }
            Self::RootPackage { detail } => {
                write!(f, "package.json {detail}, which affects every project")
            }
            Self::TsconfigPath {
                file,
                mapping,
                path,
            } => write!(f, "{file} path mapping {mapping} points into it ({path})"),
            Self::Tsconfig { file } => write!(
                f,
                "{file} changed outside compilerOptions.paths, which affects every project"
            ),
        }
    }
}

/// What changed of a project's configuration, as a reason names it: its
/// targets and named inputs by name, then its other keys.
fn changed_parts(targets: &[String], named_inputs: &[String], fields: &[String]) -> String {
    let mut changed = Vec::new();
    for (noun, names) in [("target", targets), ("named input", named_inputs)] {
        match names {
            [] => {}
            [name] => changed.push(format!("{noun} {name}")),
            names => changed.push(format!("{noun}s {}", names.join(", "))),
        }
    }
    changed.extend(fields.iter().cloned());
    changed.join(" and ")
}

pub fn analyse(workspace: &Workspace, graph: &ProjectGraph, options: &Options) -> Result<Analysis> {
    let changes = Changes::new(workspace, options)?;
    let mut touches = Touches::default();
    for locator in [
        touched_by_path,
        touched_by_manifests,
        touched_by_nx_json,
        touched_implicitly,
        touched_by_deleted_manifests,
        touched_by_lockfile,
        touched_by_root_package,
        touched_by_root_tsconfig,
    ] {
        locator(workspace, &changes, &mut touches)?;
    }
    for project in touches.0.keys() {
        if !workspace.projects.contains_key(project) {
            bail!("invalid project name {project:?}");
        }
    }
    let mut projects = propagate(graph, &touches.0, &BTreeSet::new());
    let mut reachability = BTreeMap::new();
    let mut left_out = BTreeMap::new();
    if let Some(profile) = &options.affected_profile
        && let Some(reach) = projections::reachability(workspace, profile)?
    {
        // A project is decided by its tasks: every target of it, with what
        // they depend on. One without a narrowed target cannot be left out,
        // so its tasks are not looked at.
        let configs = reachability::Configs::load(workspace, reach)?;
        let requests: Vec<qk_taskgraph::Request> = projects
            .keys()
            .filter(|project| {
                workspace.projects[*project]
                    .targets
                    .values()
                    .any(|target| configs.narrows(&target.extra))
            })
            .flat_map(|project| {
                workspace.projects[project]
                    .targets
                    .keys()
                    .map(|target| qk_taskgraph::Request {
                        project: project.clone(),
                        target: target.clone(),
                        configuration: None,
                        requested_configuration: None,
                        args: Vec::new(),
                    })
            })
            .collect();
        if !requests.is_empty() {
            let tasks = qk_taskgraph::TaskGraph::build(workspace, &requests)?;
            let analysis = tasks::analyse(
                workspace,
                &tasks,
                Changes::unfiltered(workspace, options)?,
                Some(&configs),
            )?;
            reachability = reachability::decide_projects(&tasks, &analysis, &projects);
            for (name, decision) in &reachability {
                if matches!(decision, Decision::LeftOut { .. })
                    && let Some(cause) = projects.remove(name)
                {
                    left_out.insert(name.clone(), cause);
                }
            }
        }
    }
    Ok(Analysis {
        projections: changes.projections,
        original_files: changes.original_files,
        base: changes.base,
        head: changes.head,
        range: changes.range,
        files: changes.files,
        projects,
        reachability,
        left_out,
    })
}

/// The touched projects and every project depending on one, except through
/// `skipped`, which are left out with whatever only they lead to.
fn propagate(
    graph: &ProjectGraph,
    touched: &BTreeMap<String, Vec<Reason>>,
    skipped: &BTreeSet<String>,
) -> BTreeMap<String, Cause> {
    let mut dependents: BTreeMap<&str, Vec<(&str, &str)>> = BTreeMap::new();
    for (source, edges) in &graph.dependencies {
        for edge in edges {
            dependents
                .entry(&edge.target)
                .or_default()
                .push((source, &edge.kind));
        }
    }
    let mut projects = BTreeMap::new();
    let mut pending = std::collections::VecDeque::new();
    for (project, reasons) in touched {
        pending.push_back(project.clone());
        projects.insert(
            project.clone(),
            Cause::Touched {
                reasons: reasons.clone(),
            },
        );
    }
    // Breadth first, so each dependent records a step of a shortest path.
    while let Some(project) = pending.pop_front() {
        for (dependent, kind) in dependents.get(project.as_str()).into_iter().flatten() {
            if !projects.contains_key(*dependent) && !skipped.contains(*dependent) {
                projects.insert(
                    (*dependent).to_owned(),
                    Cause::DependsOn {
                        project: project.clone(),
                        kind: (*kind).to_owned(),
                        also: Vec::new(),
                    },
                );
                pending.push_back((*dependent).to_owned());
            }
        }
    }
    let affected: BTreeSet<String> = projects.keys().cloned().collect();
    for (name, cause) in &mut projects {
        if let Cause::DependsOn { project, also, .. } = cause {
            *also = graph.dependencies[name]
                .iter()
                .map(|edge| &edge.target)
                .filter(|target| *target != project && affected.contains(*target))
                .cloned()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
        }
    }
    projects
}

impl Analysis {
    /// The path from `project` to the touched project it is affected through,
    /// starting with `project`, or `None` when it is not affected.
    pub fn chain(&self, project: &str) -> Option<Vec<&str>> {
        let mut chain = vec![self.projects.get_key_value(project)?.0.as_str()];
        while let Some(Cause::DependsOn { project, .. }) = self.cause(chain.last().unwrap()) {
            chain.push(project);
        }
        Some(chain)
    }

    /// Why `project` is affected, also when the profile left it out.
    pub fn cause(&self, project: &str) -> Option<&Cause> {
        self.projects
            .get(project)
            .or_else(|| self.left_out.get(project))
    }
}

/// Reasons by touched project.
#[derive(Default)]
struct Touches(BTreeMap<String, Vec<Reason>>);

impl Touches {
    fn touch(&mut self, project: &str, reason: Reason) {
        self.0.entry(project.to_owned()).or_default().push(reason);
    }

    fn all(&mut self, workspace: &Workspace, reason: Reason) {
        for project in workspace.projects.keys() {
            self.touch(project, reason.clone());
        }
    }
}

/// How the base of a commit range was found, and what the range holds.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Range {
    /// The base as given, such as `main`.
    pub requested: String,
    /// The upstream the merge base was taken from instead, when the head
    /// left it later than it left `requested`: a local `main` behind
    /// `origin/main` would otherwise count what landed since as changes.
    pub upstream: Option<String>,
    /// Commits from the merge base to the head.
    pub commits: usize,
    /// Of those, the commits already on the default branch, with the ref they
    /// were found on. Their changes count as the head's own; always zero when
    /// the head itself is on the default branch.
    pub landed: usize,
    pub default_branch: Option<String>,
}

/// The changed files, and each one's content at the two revisions.
struct Changes<'a> {
    projections: Vec<ProjectionReport>,
    original_files: Option<Vec<String>>,
    workspace: &'a Workspace,
    before_lockfile: OnceCell<std::result::Result<Lockfile, String>>,
    after_lockfile: OnceCell<std::result::Result<Lockfile, String>>,
    manifests: OnceCell<manifests::ManifestChange>,
    files: Vec<String>,
    /// The files were named rather than compared, as with `--files`: each
    /// counts as changed whole, as in Nx.
    whole: bool,
    base: Option<String>,
    /// `None` reads the working tree.
    head: Option<String>,
    range: Option<Range>,
}

enum FileChange {
    Deleted,
    /// Nothing is known about what changed inside the file.
    Whole,
    Json(Vec<Change>),
    Lockfile {
        before: String,
        after: String,
    },
}

impl<'a> Changes<'a> {
    fn new(workspace: &'a Workspace, options: &Options) -> Result<Self> {
        ensure!(
            options.fail_on_projection_fallback.is_none() || options.affected_profile.is_some(),
            "--fail-on-projection-fallback requires --affected-profile"
        );
        if options.fail_on_projection_fallback.is_some() {
            ensure!(
                !options.explicit_files
                    && options.files.is_empty()
                    && !options.uncommitted
                    && !options.untracked,
                "affected profiles require a committed base/head comparison; --files, --stdin, --uncommitted and --untracked cannot be combined with strict projection selection"
            );
            let head = options.head.clone().or_else(|| non_empty_env("NX_HEAD"));
            ensure!(
                head.as_ref().is_some_and(|head| !head.trim().is_empty()),
                "affected profiles require --head (or NX_HEAD) for strict projection selection"
            );
        }
        let mut changes = Self::unfiltered(workspace, options)?;
        let ignore = qk_cache::SourceIgnore::new(&workspace.root)?;
        changes.files.retain(|file| !ignore.matches(file));
        if let Some(profile) = &options.affected_profile {
            let (files, reports) = projections::apply(workspace, options, &changes, profile)?;
            // A profile without projections, only narrowing by reachability,
            // leaves the changes as they are.
            if !reports.is_empty() {
                changes.original_files = Some(std::mem::replace(&mut changes.files, files));
            }
            changes.projections = reports;
        }
        Ok(changes)
    }

    /// Task inputs can explicitly read ignored JSON and mandatory metadata.
    /// Let the cache resolver decide which changed paths are inputs.
    fn unfiltered(workspace: &'a Workspace, options: &Options) -> Result<Self> {
        let root = &workspace.root;
        let head = options.head.clone().or_else(|| non_empty_env("NX_HEAD"));
        let default_base = workspace
            .config
            .default_base
            .clone()
            .or_else(|| {
                workspace
                    .config
                    .extra
                    .get("affected")
                    .and_then(|affected| affected.get("defaultBase"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| "main".into());
        let requested = options
            .base
            .clone()
            .or_else(|| non_empty_env("NX_BASE"))
            .unwrap_or_else(|| default_base.clone());
        // As Nx's `assertValidGitRevision`: git would read one as an option.
        for revision in std::iter::once(&requested).chain(&head) {
            ensure!(
                !revision.starts_with('-'),
                "invalid git revision {revision:?}: git revisions cannot start with \"-\""
            );
        }
        let head_revision = head.as_deref().unwrap_or("HEAD");
        let (merged, upstream) = merge_base(root, &requested, head_revision);
        let range = (options.files.is_empty()
            && !options.explicit_files
            && !options.uncommitted
            && !options.untracked)
            .then(|| {
                // The default branch only qualifies the range, so one shaped
                // like an option is not looked up rather than refused.
                let default_branch = (!default_base.starts_with('-'))
                    .then(|| {
                        upstream_of(root, &default_base).or_else(|| {
                            git_lines(root, &["rev-parse", "--verify", "--quiet", &default_base])
                                .ok()
                                .map(|_| default_base.clone())
                        })
                    })
                    .flatten();
                Range {
                    commits: count(root, &merged, head_revision),
                    landed: default_branch
                        .as_deref()
                        .map_or(0, |branch| landed(root, &merged, head_revision, branch)),
                    requested: requested.clone(),
                    upstream,
                    default_branch,
                }
            });
        let base = Some(merged);
        let whole = options.explicit_files || !options.files.is_empty();
        let files = if whole {
            options.files.clone()
        } else if options.uncommitted {
            uncommitted(root)?
        } else if options.untracked {
            untracked(root)?
        } else if let (Some(base), Some(head)) = (&base, &head) {
            git_paths(
                root,
                &[
                    "diff",
                    "--name-only",
                    "--no-renames",
                    "--relative",
                    base,
                    head,
                ],
            )?
        } else {
            let base = base.as_deref().expect("base defaults to main");
            let mut files: BTreeSet<String> = git_paths(
                root,
                &[
                    "diff",
                    "--name-only",
                    "--no-renames",
                    "--relative",
                    base,
                    "HEAD",
                ],
            )?
            .into_iter()
            .collect();
            files.extend(uncommitted(root)?);
            files.extend(untracked(root)?);
            files.into_iter().collect()
        };
        Ok(Self {
            projections: Vec::new(),
            original_files: None,
            workspace,
            before_lockfile: OnceCell::new(),
            after_lockfile: OnceCell::new(),
            manifests: OnceCell::new(),
            files,
            whole,
            base,
            head,
            range,
        })
    }

    fn changed(&self, file: &str) -> bool {
        self.files.iter().any(|changed| changed == file)
    }

    /// The changed project manifests counted by what they change.
    fn manifests(&self) -> &manifests::ManifestChange {
        self.manifests
            .get_or_init(|| manifests::ManifestChange::new(self.workspace, self))
    }

    /// Nx's `calculateFileChanges` for one file.
    fn change(&self, file: &str) -> FileChange {
        if !self.workspace.root.join(file).exists() {
            return FileChange::Deleted;
        }
        if self.whole {
            return FileChange::Whole;
        }
        let read = || -> Option<(String, String)> {
            Some((
                self.read(file, self.base.as_deref())?,
                self.read(file, self.head.as_deref())?,
            ))
        };
        let name = file.rsplit('/').next().unwrap_or(file);
        if LOCKFILES.contains(&name) {
            return match read() {
                Some((before, after)) => FileChange::Lockfile { before, after },
                None => FileChange::Whole,
            };
        }
        let parsed = if file.ends_with(".json") {
            read().and_then(|(before, after)| {
                Some((
                    serde_json::from_str(&before).ok()?,
                    serde_json::from_str(&after).ok()?,
                ))
            })
        } else if file.ends_with(".yaml") || file.ends_with(".yml") {
            read().and_then(|(before, after)| {
                Some((
                    serde_yaml_ng::from_str(&before).ok()?,
                    serde_yaml_ng::from_str(&after).ok()?,
                ))
            })
        } else {
            None
        };
        match parsed {
            Some((before, after)) => FileChange::Json(json_diff::diff(&before, &after)),
            None => FileChange::Whole,
        }
    }

    fn lockfile(&self, before: bool) -> std::result::Result<&Lockfile, &str> {
        let (cell, revision) = if before {
            (&self.before_lockfile, self.base.as_deref())
        } else {
            (&self.after_lockfile, self.head.as_deref())
        };
        cell.get_or_init(|| {
            let text = self
                .read("pnpm-lock.yaml", revision)
                .ok_or_else(|| "a revision could not be read".to_owned())?;
            Lockfile::parse(&text).map_err(|error| format!("{error:#}"))
        })
        .as_ref()
        .map_err(String::as_str)
    }

    fn read(&self, file: &str, revision: Option<&str>) -> Option<String> {
        match revision {
            None => std::fs::read_to_string(self.workspace.root.join(file)).ok(),
            Some(revision) => {
                let output = workspace_command("git")
                    .current_dir(&self.workspace.root)
                    .args(["show", &format!("{revision}:./{file}")])
                    .output()
                    .ok()?;
                output
                    .status
                    .success()
                    .then(|| String::from_utf8(output.stdout).ok())
                    .flatten()
            }
        }
    }
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// Where `head` left `base`, as Nx finds it, unless `base` is a branch whose
/// upstream `head` left later. Then that later point is the base, with the
/// upstream's name.
fn merge_base(root: &Path, base: &str, head: &str) -> (String, Option<String>) {
    let first = |args: &[&str]| {
        git_lines(root, args)
            .ok()
            .and_then(|lines| lines.into_iter().next())
    };
    let local = first(&["merge-base", base, head])
        .or_else(|| first(&["merge-base", "--fork-point", base, head]))
        .unwrap_or_else(|| base.to_owned());
    if let Some(upstream) = upstream_of(root, base)
        && let Some(remote) = first(&["merge-base", &upstream, head])
        && remote != local
        && is_ancestor(root, &local, &remote)
    {
        return (remote, Some(upstream));
    }
    (local, None)
}

/// The remote-tracking branch `branch` follows, such as `origin/main`.
fn upstream_of(root: &Path, branch: &str) -> Option<String> {
    git_lines(
        root,
        &[
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            &format!("{branch}@{{upstream}}"),
        ],
    )
    .ok()?
    .into_iter()
    .next()
}

fn is_ancestor(root: &Path, ancestor: &str, descendant: &str) -> bool {
    workspace_command("git")
        .current_dir(root)
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .output()
        .is_ok_and(|output| output.status.success())
}

fn count(root: &Path, from: &str, to: &str) -> usize {
    git_lines(root, &["rev-list", "--count", &format!("{from}..{to}")])
        .ok()
        .and_then(|lines| lines.first()?.parse().ok())
        .unwrap_or(0)
}

/// Commits between `base` and `head` that `branch` already holds, unless
/// `head` is on `branch` itself, where the range is the branch's own history.
fn landed(root: &Path, base: &str, head: &str, branch: &str) -> usize {
    if is_ancestor(root, head, branch) {
        return 0;
    }
    match git_lines(root, &["merge-base", branch, head]) {
        Ok(lines) => match lines.first() {
            Some(shared) if shared != base && is_ancestor(root, base, shared) => {
                count(root, base, shared)
            }
            _ => 0,
        },
        Err(_) => 0,
    }
}

fn uncommitted(root: &Path) -> Result<Vec<String>> {
    git_paths(
        root,
        &[
            "diff",
            "--name-only",
            "--no-renames",
            "--relative",
            "HEAD",
            ".",
        ],
    )
}

/// Untracked files as Nx's `getUntrackedFiles` lists them: what Git does not
/// ignore. An `.nxignore` negation cannot bring back a file `.gitignore`
/// excludes, since it filters this list and does not add to it.
fn untracked(root: &Path) -> Result<Vec<String>> {
    git_paths(root, &["ls-files", "--others", "--exclude-standard"])
}

/// The paths a Git command lists, read NUL-separated, as Nx 24 reads them,
/// so that Git does not quote names outside ASCII.
fn git_paths(root: &Path, args: &[&str]) -> Result<Vec<String>> {
    let (command, rest) = args.split_first().context("a git command")?;
    let mut arguments = vec![*command, "-z"];
    arguments.extend(rest);
    let output = workspace_command("git")
        .current_dir(root)
        .args(&arguments)
        .output()
        .context("could not run git")?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| String::from_utf8(path.to_vec()).context("git paths must be UTF-8"))
        .collect()
}

fn git_lines(root: &Path, args: &[&str]) -> Result<Vec<String>> {
    let output = workspace_command("git")
        .current_dir(root)
        .args(args)
        .output()
        .context("could not run git")?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8(output.stdout)
        .context("git output must be UTF-8")?
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect())
}

/// Each project by its root, to find the project owning a path.
pub(crate) struct ProjectRoots<'w>(BTreeMap<&'w str, &'w str>);

impl<'w> ProjectRoots<'w> {
    pub(crate) fn new(workspace: &'w Workspace) -> Self {
        Self(
            workspace
                .projects
                .values()
                .map(|project| (project.root.as_str(), project.name.as_str()))
                .collect(),
        )
    }

    /// The project whose root most specifically contains `path`.
    pub(crate) fn owner(&self, path: &str) -> Option<&'w str> {
        let mut path = path;
        loop {
            if let Some(project) = self.0.get(path) {
                return Some(project);
            }
            match path.rfind('/') {
                Some(slash) => path = &path[..slash],
                None if path != "." => path = ".",
                None => return None,
            }
        }
    }
}

/// Each file touches the project whose root most specifically contains it,
/// but for a project manifest counted by what it changes, which touches its
/// project so only where a task of the project reads it, as a target's
/// default inputs do.
fn touched_by_path(workspace: &Workspace, changes: &Changes, touches: &mut Touches) -> Result<()> {
    let roots = ProjectRoots::new(workspace);
    let compared = &changes.manifests().files;
    let read = manifests_read(workspace, &roots, compared);
    for file in &changes.files {
        let Some(project) = roots.owner(file) else {
            continue;
        };
        if compared.contains(file) && !read.contains(file) {
            continue;
        }
        touches.touch(project, Reason::File { file: file.clone() });
    }
    Ok(())
}

/// The manifests of `compared` a task of their own project reads, with its
/// inputs resolved as its cache key resolves them; all of them where the
/// tasks cannot be resolved.
fn manifests_read(
    workspace: &Workspace,
    roots: &ProjectRoots,
    compared: &BTreeSet<String>,
) -> BTreeSet<String> {
    let owners: BTreeMap<&String, &str> = compared
        .iter()
        .filter_map(|file| Some((file, roots.owner(file)?)))
        .collect();
    let projects: BTreeSet<&str> = owners.values().copied().collect();
    let requests: Vec<qk_taskgraph::Request> = projects
        .iter()
        .flat_map(|project| {
            workspace.projects[*project]
                .targets
                .keys()
                .map(|target| qk_taskgraph::Request {
                    project: (*project).to_owned(),
                    target: target.clone(),
                    configuration: None,
                    requested_configuration: None,
                    args: Vec::new(),
                })
        })
        .collect();
    if requests.is_empty() {
        return BTreeSet::new();
    }
    let resolved = (|| -> Result<_> {
        let graph = qk_taskgraph::TaskGraph::build_with(
            workspace,
            &requests,
            qk_taskgraph::BuildOptions {
                exclude_task_dependencies: true,
                ..Default::default()
            },
        )?;
        let inputs = qk_cache::resolve_tasks(workspace, &graph, &[])?;
        Ok((graph, inputs))
    })();
    let Ok((graph, inputs)) = resolved else {
        return compared.clone();
    };
    owners
        .into_iter()
        .filter(|(file, owner)| {
            inputs.iter().any(|(id, resolved)| {
                graph.tasks[id].project == *owner
                    && resolved.file_inputs().any(|input| input == file.as_str())
            })
        })
        .map(|(file, _)| file.clone())
        .collect()
}

/// A `project.json` or `project.local.json` present at both revisions
/// touches the projects whose configuration it changes, any project's, since
/// tags and names decide what other projects' inputs and dependencies
/// select. One that cannot be compared touches its project as any file does.
fn touched_by_manifests(
    workspace: &Workspace,
    changes: &Changes,
    touches: &mut Touches,
) -> Result<()> {
    let change = changes.manifests();
    let Some(base) = &change.base else {
        return Ok(());
    };
    let files: Vec<String> = change.files.iter().cloned().collect();
    for (project, difference) in manifests::differences(workspace, base) {
        touches.touch(
            &project,
            Reason::ProjectConfiguration {
                files: files.clone(),
                targets: difference.targets,
                named_inputs: difference.named_inputs,
                fields: difference.fields,
            },
        );
    }
    Ok(())
}

/// `nx.json`, the file it extends or `nx.local.json` touches what it
/// changes: every project when it changed what every task's key holds, and
/// otherwise the projects whose targets or named inputs `targetDefaults` and
/// `namedInputs` resolve differently. Nx 24 touches every project for any
/// change to `nx.json`, and none for the file it extends.
fn touched_by_nx_json(
    workspace: &Workspace,
    changes: &Changes,
    touches: &mut Touches,
) -> Result<()> {
    match nx_json::NxJsonChange::new(workspace, changes) {
        nx_json::NxJsonChange::None => {}
        nx_json::NxJsonChange::All { file, detail } => {
            touches.all(workspace, Reason::NxJson { file, detail });
        }
        nx_json::NxJsonChange::Resolved {
            file,
            settings,
            base,
        } => {
            for project in workspace.projects.values() {
                let before = base.projects.get(&project.name);
                let targets = changed_names(&project.targets, before.map(|before| &before.targets));
                let named_inputs = changed_names(
                    &project.named_inputs,
                    before.map(|before| &before.named_inputs),
                );
                if targets.is_empty() && named_inputs.is_empty() {
                    continue;
                }
                touches.touch(
                    &project.name,
                    Reason::NxJsonConfiguration {
                        file: file.clone(),
                        settings: settings
                            .iter()
                            .map(|setting| (*setting).to_owned())
                            .collect(),
                        targets,
                        named_inputs,
                    },
                );
            }
        }
    }
    Ok(())
}

/// The names whose entries differ between `after` and `before`, every name
/// of `after` when there is no `before`.
fn changed_names<T: Serialize>(
    after: &BTreeMap<String, T>,
    before: Option<&BTreeMap<String, T>>,
) -> Vec<String> {
    let value = |entry: Option<&T>| entry.map(|entry| serde_json::to_value(entry).ok());
    let empty = BTreeMap::new();
    let before = before.unwrap_or(&empty);
    after
        .keys()
        .chain(before.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|name| value(after.get(*name)) != value(before.get(*name)))
        .cloned()
        .collect()
}

/// A file named by a `{workspaceRoot}` input touches the projects declaring
/// it, `nx.json` among them.
fn touched_implicitly(
    workspace: &Workspace,
    changes: &Changes,
    touches: &mut Touches,
) -> Result<()> {
    // Each glob with the targets declaring it, in the order Nx finds them.
    let mut patterns: BTreeMap<String, Vec<(&str, &str)>> = BTreeMap::new();
    for project in workspace.projects.values() {
        for (name, target) in &project.targets {
            let mut globs = Vec::new();
            workspace_inputs(
                target.inputs.as_deref().unwrap_or_default(),
                &project.named_inputs,
                &mut globs,
                0,
            );
            for glob in globs {
                patterns
                    .entry(glob)
                    .or_default()
                    .push((&project.name, name));
            }
        }
    }
    for (pattern, declarations) in patterns {
        let Some(file) = changes
            .files
            .iter()
            .find(|file| implicit_match(&pattern, file))
        else {
            continue;
        };
        for (project, target) in declarations {
            touches.touch(
                project,
                Reason::WorkspaceInput {
                    file: file.clone(),
                    target: target.to_owned(),
                    input: format!("{{workspaceRoot}}/{pattern}"),
                },
            );
        }
    }
    Ok(())
}

/// Whether a changed file matches a `{workspaceRoot}` input, as Nx's
/// `implicitly_touched_projects` reads it: repeated and trailing slashes
/// collapsed, and a path without glob syntax naming that file or everything
/// under it. A glob that cannot be compiled is taken literally.
fn implicit_match(pattern: &str, file: &str) -> bool {
    let mut normalized = String::with_capacity(pattern.len());
    for c in pattern.chars() {
        if !(c == '/' && normalized.ends_with('/')) {
            normalized.push(c);
        }
    }
    while normalized.len() > 1 && normalized.ends_with('/') {
        normalized.pop();
    }
    let literal = |path: &str| {
        file == path
            || file
                .strip_prefix(path)
                .is_some_and(|rest| rest.starts_with('/'))
    };
    // `@` and `+` are syntax only before a group, so `@types` is a name.
    if !normalized.contains(['*', '?', '[', ']', '{', '}', '(', ')', '\\']) {
        return literal(&normalized);
    }
    match Pattern::new(&normalized, false) {
        Ok(matcher) => matcher.is_match(file),
        Err(_) => literal(&normalized),
    }
}

/// Nx's `extractFilesFromInputs`: named inputs are followed, and plain or
/// `fileset` globs under `{workspaceRoot}/` are collected without the prefix.
fn workspace_inputs(
    inputs: &[Value],
    named: &BTreeMap<String, Vec<Value>>,
    globs: &mut Vec<String>,
    depth: usize,
) {
    // Nx would recurse forever on a named input cycle; qk stops.
    if depth > 32 {
        return;
    }
    for input in inputs {
        let glob = match input {
            Value::String(name) if named.contains_key(name) => {
                workspace_inputs(&named[name], named, globs, depth + 1);
                continue;
            }
            Value::String(glob) => glob,
            Value::Object(object) => match object.get("fileset").and_then(Value::as_str) {
                Some(glob) => glob,
                None => continue,
            },
            _ => continue,
        };
        if let Some(glob) = glob.strip_prefix("{workspaceRoot}/") {
            globs.push(glob.to_owned());
        }
    }
}

/// A deleted file that projects are created from may have removed a project.
/// Nx then touches every project, having no way of knowing what depended on
/// it. qk's graph is made of manifests alone, and a project whose files the
/// change leaves alone has the same manifests at both revisions, so what
/// depended on a removed project is what still declares it, read at the
/// head; a project whose files changed is touched already.
fn touched_by_deleted_manifests(
    workspace: &Workspace,
    changes: &Changes,
    touches: &mut Touches,
) -> Result<()> {
    match removed(workspace, changes, &changes.files) {
        Removal::None => {}
        Removal::Unknown { file, detail } => {
            touches.all(workspace, Reason::DeletedManifest { file, detail });
        }
        Removal::Dependents(dependents) => {
            for (dependent, (file, project)) in dependents {
                touches.touch(&dependent, Reason::DependedOnRemoved { file, project });
            }
        }
    }
    Ok(())
}

/// What deleting files projects are created from removed from the graph.
pub(crate) enum Removal {
    /// No project: none was deleted, or each deleted one is still a project.
    None,
    /// qk cannot tell what depended on what was removed.
    Unknown { file: String, detail: String },
    /// Each project that depended on a removed project or package, with the
    /// deleted file and the name of what it removed.
    Dependents(BTreeMap<String, (String, String)>),
}

/// What the deleted manifests among `files` removed: the projects at the
/// base revision that the head no longer has, and what depended on them.
pub(crate) fn removed(workspace: &Workspace, changes: &Changes, files: &[String]) -> Removal {
    let deleted: Vec<&String> = files
        .iter()
        .filter(|file| is_project_config(file) && !workspace.root.join(file).exists())
        .collect();
    let Some(first) = deleted.first() else {
        return Removal::None;
    };
    let unknown = |file: &str, detail: &str| Removal::Unknown {
        file: file.to_owned(),
        detail: detail.to_owned(),
    };
    if let Some(file) = deleted
        .iter()
        .find(|file| PLUGIN_LOCKFILES.contains(&file.as_str()))
    {
        return unknown(file, "it configures every project");
    }
    let Some(base) = changes.base.as_deref().filter(|_| !changes.whole) else {
        return unknown(first, "the files were named rather than compared");
    };
    // The head graph, whose edges say which links a deletion left in place.
    let head = match ProjectGraph::build(workspace) {
        Ok(graph) => graph,
        Err(error) => {
            return unknown(
                first,
                &format!("the head graph could not be built ({error:#})"),
            );
        }
    };
    let mut dependents = BTreeMap::new();
    // The head's projects, each removed one added in turn, as the base had it.
    let mut projects = workspace.projects.clone();
    // Each directory once, whichever of its manifests went.
    let mut directories: BTreeMap<&str, &String> = BTreeMap::new();
    for file in &deleted {
        directories
            .entry(
                file.rsplit_once('/')
                    .map_or(".", |(directory, _)| directory),
            )
            .or_insert(file);
    }
    for (directory, file) in directories {
        let read = |name: &str| {
            let path = match directory {
                "." => name.to_owned(),
                directory => format!("{directory}/{name}"),
            };
            changes.read(&path, Some(base))
        };
        let (project_json, package_json) = (read("project.json"), read("package.json"));
        let project = match workspace.project_from_manifests(
            directory,
            project_json.as_deref(),
            package_json.as_deref(),
        ) {
            Ok(Some(project)) => project,
            Ok(None) => return unknown(file, "it could not be read at the base revision"),
            Err(error) => {
                return unknown(
                    file,
                    &format!("it could not be read at the base revision ({error:#})"),
                );
            }
        };
        let (removed, package) = project;
        // Each way to depend on it goes only where the head lost it: a
        // project still there, by its other manifest or moved under its
        // name, keeps what names it, and a package still there what links
        // to it, though either may be what the deletion removed alone.
        let mut depended = |dependent: &str, what: &str| {
            dependents
                .entry(dependent.to_owned())
                .or_insert_with(|| ((*file).clone(), what.to_owned()));
        };
        let added = !projects.contains_key(&removed.name);
        if added {
            projects.insert(removed.name.clone(), removed.clone());
        }
        // A dependent's own implicitDependencies, as at the base: its files
        // are the same at both revisions unless the change touches it.
        let implicit = |dependent: &str| {
            qk_graph::expand_implicit_dependencies(
                dependent,
                &workspace.projects[dependent].implicit_dependencies,
                &projects,
            )
        };
        let excluded = format!("!{}", removed.name);
        if workspace.analyzes_package_json()
            && let Some(package) = &package
            && let Some(name) = &package.name
        {
            for (dependent, manifest) in &workspace.packages {
                // The edge the base graph had: linked by its range to where
                // the package was, unless excluded.
                let linked = manifest
                    .dependency_ranges()
                    .get(name.as_str())
                    .is_some_and(|range| {
                        qk_graph::links(
                            range,
                            &workspace.projects[dependent].root,
                            &removed.root,
                            package.version.as_deref(),
                        )
                    })
                    && !implicit(dependent).contains(&excluded);
                // Kept only where the head graph has the same edge, as when
                // the package moved and its range still links to it.
                let kept = head
                    .dependencies
                    .get(dependent)
                    .into_iter()
                    .flatten()
                    .any(|edge| edge.target == removed.name && edge.kind == "static");
                if linked && !kept {
                    depended(dependent, name);
                }
            }
        }
        // Still a project by that name, what names it still selects it.
        if workspace.projects.contains_key(&removed.name) {
            continue;
        }
        for (dependent, project) in &workspace.projects {
            let implicit = qk_graph::expand_implicit_dependencies(
                dependent,
                &project.implicit_dependencies,
                &projects,
            );
            let excluded = format!("!{}", removed.name);
            if implicit.contains(&removed.name) && !implicit.contains(&excluded) {
                depended(dependent, &removed.name);
            }
        }
        // Only a name the head lacks was added, and so reaches here.
        if added {
            projects.remove(&removed.name);
        }
    }
    Removal::Dependents(dependents)
}

fn touched_by_lockfile(
    workspace: &Workspace,
    changes: &Changes,
    touches: &mut Touches,
) -> Result<()> {
    let Some(file) = changed_lockfile(changes) else {
        return Ok(());
    };
    let setting = projects_affected_by_dependency_updates(workspace);
    let reason = Reason::DependencyUpdates {
        file: file.clone(),
        setting: setting.clone(),
    };
    match DependencyUpdates::read(workspace)? {
        DependencyUpdates::Auto => {}
        DependencyUpdates::Projects(projects) => {
            for project in projects {
                touches.touch(&project, reason.clone());
            }
            return Ok(());
        }
        DependencyUpdates::All => {
            touches.all(workspace, reason);
            return Ok(());
        }
    }
    let unreadable = |detail: &str| Reason::UnreadableLockfile {
        file: file.clone(),
        detail: detail.to_owned(),
    };
    if changes.whole {
        touches.all(
            workspace,
            unreadable("its revisions are not compared for --files"),
        );
        return Ok(());
    }
    if file != "pnpm-lock.yaml" {
        let detail = match changes.change(file) {
            FileChange::Lockfile { .. } => "qk reads pnpm lockfiles only",
            _ => "a revision could not be read",
        };
        touches.all(workspace, unreadable(detail));
        return Ok(());
    }
    if !workspace.root.join(file).exists() {
        touches.all(workspace, unreadable("a revision could not be read"));
        return Ok(());
    }
    let (before, after) = match (changes.lockfile(true), changes.lockfile(false)) {
        (Ok(before), Ok(after)) => (before, after),
        (Err(error), _) | (_, Err(error)) => {
            touches.all(workspace, unreadable(error));
            return Ok(());
        }
    };
    for project in workspace.projects.values() {
        let (old, new) = (
            before.installation(&project.root),
            after.installation(&project.root),
        );
        if old == new {
            continue;
        }
        let empty = || Installation {
            direct: BTreeMap::new(),
            snapshots: BTreeMap::new(),
        };
        let (old, new) = (old.unwrap_or_else(empty), new.unwrap_or_else(empty));
        let direct = old
            .direct
            .keys()
            .chain(new.direct.keys())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter(|name| old.direct.get(*name) != new.direct.get(*name))
            .map(|name| {
                let show = |key: Option<&String>| key.map_or("(none)", String::as_str).to_owned();
                format!(
                    "{name}: {} -> {}",
                    show(old.direct.get(name)),
                    show(new.direct.get(name))
                )
            })
            .collect();
        let only = |one: &Installation, other: &Installation| -> Vec<String> {
            one.snapshots
                .keys()
                .filter(|key| !other.snapshots.contains_key(*key))
                .cloned()
                .collect()
        };
        touches.touch(
            &project.name,
            Reason::Installs {
                file: file.clone(),
                importer: project.root.clone(),
                direct,
                added: only(&new, &old),
                removed: only(&old, &new),
                changed: old
                    .snapshots
                    .iter()
                    .filter(|(key, fingerprint)| {
                        new.snapshots
                            .get(*key)
                            .is_some_and(|other| other != *fingerprint)
                    })
                    .map(|(key, _)| key.clone())
                    .collect(),
            },
        );
    }
    Ok(())
}

/// The changed root lockfile, as Nx finds it.
pub(crate) fn changed_lockfile<'c>(changes: &'c Changes) -> Option<&'c String> {
    changes
        .files
        .iter()
        .find(|file| LOCKFILES.contains(&file.as_str()))
}

/// What `projectsAffectedByDependencyUpdates` makes of a lockfile change.
pub(crate) enum DependencyUpdates {
    /// Compare what changed in it.
    Auto,
    /// These projects, whatever changed.
    Projects(BTreeSet<String>),
    /// Every project, or every task hashing installed packages: Nx's default.
    All,
}

impl DependencyUpdates {
    fn read(workspace: &Workspace) -> Result<Self> {
        Ok(match projects_affected_by_dependency_updates(workspace) {
            // Without the lockfile's packages in the graph, Nx's `auto` finds
            // no package that changed and takes every project.
            Value::String(mode) if mode == "auto" && workspace.analyzes_lockfile() => Self::Auto,
            Value::Array(selectors) => {
                let selectors: Vec<String> = serde_json::from_value(Value::Array(selectors))
                    .context("projectsAffectedByDependencyUpdates must list project selectors")?;
                Self::Projects(
                    qk_config::find_matching_projects(
                        &selectors,
                        &workspace.projects,
                        qk_config::Reader::Nx,
                    )
                    .into_iter()
                    .collect(),
                )
            }
            _ => Self::All,
        })
    }
}

/// `pluginsConfig["@nx/js"].projectsAffectedByDependencyUpdates`, which Nx
/// defaults to `"all"`.
pub(crate) fn projects_affected_by_dependency_updates(workspace: &Workspace) -> Value {
    workspace
        .config
        .extra
        .get("pluginsConfig")
        .and_then(|config| config.get("@nx/js"))
        .and_then(|config| config.get("projectsAffectedByDependencyUpdates"))
        .cloned()
        .unwrap_or_else(|| Value::String("all".into()))
}

/// What a root `package.json` change moves, as Nx's `touchedNpmPackages`
/// names it.
pub(crate) enum NpmChanges {
    /// It cannot be pinned to packages, which moves every one.
    Everything { detail: String },
    Named {
        /// Each installed package it moved, with the path that moved it.
        packages: BTreeMap<String, String>,
        /// The workspace projects it names as dependencies, each with its path.
        projects: BTreeMap<String, String>,
        /// The file changed whole, which moves every installed package.
        all_packages: bool,
    },
}

/// What a root `package.json` change moves, or `None` when it did not
/// change. Nx diffs its dependencies and overrides: a removed dependency, a
/// global package (`nx` or a plugin nx.json loads), or an override naming
/// nothing installed moves everything; another dependency or override moves
/// the package it names, `@types/x` also moving `x`, and a dependency on a
/// workspace project names that project. Installed packages are those of the
/// head revision of `pnpm-lock.yaml`; without one qk can read, any name that
/// is not a workspace project counts as installed.
pub(crate) fn root_package_changes(workspace: &Workspace, changes: &Changes) -> Option<NpmChanges> {
    if !changes.changed("package.json") {
        return None;
    }
    let mut packages = BTreeMap::new();
    let mut projects = BTreeMap::new();
    let diff = match changes.change("package.json") {
        FileChange::Json(diff) => diff,
        // A deleted manifest says nothing about packages; the deletion counts.
        FileChange::Deleted => Vec::new(),
        _ => {
            return Some(NpmChanges::Named {
                packages,
                projects,
                all_packages: true,
            });
        }
    };
    let externals: Option<BTreeSet<String>> = changes.lockfile(false).ok().map(|lockfile| {
        lockfile
            .installations()
            .map(|installed| installed.name.to_owned())
            .collect()
    });
    let installed = |name: &str| {
        externals
            .as_ref()
            .map_or(!workspace.projects.contains_key(name), |set| {
                set.contains(name)
            })
    };
    let globals = global_packages(workspace);
    let everything = |detail: String| Some(NpmChanges::Everything { detail });
    for change in diff {
        let path: Vec<&str> = change.path.iter().map(String::as_str).collect();
        let dotted = path.join(".");
        match path.as_slice() {
            ["dependencies" | "devDependencies", name] => {
                if change.kind == Kind::Deleted {
                    return everything(format!("removed {dotted}"));
                }
                if installed(name) {
                    if globals.contains(*name) {
                        return everything(format!("changed {dotted}, which Nx treats as global"));
                    }
                    packages.insert((*name).to_owned(), dotted.clone());
                    if let Some(implementation) = name.strip_prefix("@types/")
                        && externals
                            .as_ref()
                            .is_some_and(|set| set.contains(implementation))
                    {
                        packages.insert(implementation.to_owned(), dotted.clone());
                    }
                } else if workspace.projects.contains_key(*name) {
                    projects.insert((*name).to_owned(), dotted.clone());
                }
                // Otherwise nothing installs it, and Nx moves nothing.
            }
            ["overrides" | "resolutions", ..] | ["pnpm", "overrides", ..] => {
                let pnpm = path[0] == "pnpm";
                let Some(selector) = override_selector(&change, pnpm) else {
                    continue;
                };
                let name = selector_package(selector, pnpm).filter(|name| installed(name));
                let Some(name) = name else {
                    return everything(format!(
                        "changed {dotted}, which may override a package nothing names directly"
                    ));
                };
                if globals.contains(name) {
                    return everything(format!("changed {dotted}, which Nx treats as global"));
                }
                packages.insert(name.to_owned(), dotted.clone());
            }
            _ => {}
        }
    }
    Some(NpmChanges::Named {
        packages,
        projects,
        all_packages: false,
    })
}

/// The packages Nx treats as global: `nx` and every plugin nx.json loads, by
/// package name.
fn global_packages(workspace: &Workspace) -> BTreeSet<String> {
    let plugins = workspace
        .config
        .extra
        .get("plugins")
        .and_then(Value::as_array);
    plugins
        .into_iter()
        .flatten()
        .filter_map(|plugin| match plugin {
            Value::String(plugin) => Some(plugin.as_str()),
            Value::Object(plugin) => plugin.get("plugin").and_then(Value::as_str),
            _ => None,
        })
        .map(|path| {
            // An import path's package: `@scope/name` or `name`.
            let parts = if path.starts_with('@') { 2 } else { 1 };
            path.splitn(parts + 1, '/')
                .take(parts)
                .collect::<Vec<_>>()
                .join("/")
        })
        .chain(["nx".to_owned()])
        .collect()
}

/// The selector an override change names, as Nx's `getPackageSelector`: the
/// key under `pnpm.overrides`, or otherwise the last key, a `.` standing for
/// its parent. Only a change to a version string names one.
fn override_selector(change: &Change, pnpm: bool) -> Option<&str> {
    let string = |value: &Option<Value>| value.as_ref().is_some_and(Value::is_string);
    if !string(&change.before) && !string(&change.after) {
        return None;
    }
    let index = if pnpm {
        2
    } else {
        change.path.len().checked_sub(1)?
    };
    let selector = change.path.get(index)?;
    if selector == "." {
        return change.path.get(index.checked_sub(1)?).map(String::as_str);
    }
    Some(selector)
}

/// The package an override selector names, as Nx's `findPackagesForSelector`
/// reads it: past a pnpm `parent>` prefix, the trailing `name` or
/// `@scope/name` without its `@range`.
fn selector_package(selector: &str, pnpm: bool) -> Option<&str> {
    let mut selector = selector;
    if pnpm {
        // A `>` starting a range is not a parent delimiter.
        let bytes = selector.as_bytes();
        if let Some(index) = (0..bytes.len().saturating_sub(1))
            .find(|&i| !matches!(bytes[i], b' ' | b'|' | b'@') && bytes[i + 1] == b'>')
        {
            selector = &selector[index + 2..];
        }
    }
    let part = |text: &str| {
        text.find(|c: char| matches!(c, '/' | '@' | '>') || c.is_whitespace())
            .unwrap_or(text.len())
    };
    // What follows the name: nothing, or a range without a `/`.
    let ends = |rest: &str| rest.is_empty() || (rest.starts_with('@') && !rest.contains('/'));
    let starts = std::iter::once(0).chain(
        selector
            .char_indices()
            .filter(|(_, c)| *c == '/')
            .map(|(index, _)| index + 1),
    );
    for start in starts {
        let text = &selector[start..];
        if let Some(scoped) = text.strip_prefix('@') {
            let scope = part(scoped);
            if scope > 0 && scoped[scope..].starts_with('/') {
                let name = &scoped[scope + 1..];
                let length = part(name);
                if length > 0 && ends(&name[length..]) {
                    return Some(&text[..1 + scope + 1 + length]);
                }
            }
        }
        let length = part(text);
        if length > 0 && ends(&text[length..]) {
            return Some(&text[..length]);
        }
    }
    None
}

/// Nx's `getTouchedNpmPackages`: a dependency changed in the root
/// `package.json` touches the projects installing that package.
fn touched_by_root_package(
    workspace: &Workspace,
    changes: &Changes,
    touches: &mut Touches,
) -> Result<()> {
    let Some(npm) = root_package_changes(workspace, changes) else {
        return Ok(());
    };
    let (packages, projects, all_packages) = match npm {
        NpmChanges::Everything { detail } => {
            touches.all(workspace, Reason::RootPackage { detail });
            return Ok(());
        }
        NpmChanges::Named {
            packages,
            projects,
            all_packages,
        } => (packages, projects, all_packages),
    };
    let installed = installed_packages(workspace, changes);
    if all_packages {
        // Every package is touched, so every project installing one is.
        let reason = Reason::RootPackage {
            detail: "could not be compared".into(),
        };
        match &installed {
            Some(installed) => {
                for (project, packages) in installed {
                    if !packages.is_empty() {
                        touches.touch(project, reason.clone());
                    }
                }
            }
            None => touches.all(workspace, reason),
        }
        return Ok(());
    }
    for (name, path) in packages {
        let reason = Reason::RootDependency {
            name: name.clone(),
            path: path.clone(),
        };
        match &installed {
            Some(installed) => {
                for (project, packages) in installed {
                    if packages.contains(&name) {
                        touches.touch(project, reason.clone());
                    }
                }
            }
            None => touches.all(
                workspace,
                Reason::RootPackage {
                    detail: format!(
                        "changed {path}, and qk cannot read which projects install {name}"
                    ),
                },
            ),
        }
    }
    for (name, path) in projects {
        touches.touch(
            &name,
            Reason::RootDependency {
                name: name.clone(),
                path,
            },
        );
    }
    Ok(())
}

/// The package names each project's importer installs, from the head revision
/// of `pnpm-lock.yaml`, or `None` without a lockfile qk can read.
fn installed_packages(
    workspace: &Workspace,
    changes: &Changes,
) -> Option<BTreeMap<String, BTreeSet<String>>> {
    let lockfile = changes.lockfile(false).ok()?;
    Some(
        workspace
            .projects
            .values()
            .map(|project| {
                let packages = lockfile
                    .installed_packages(&project.root)
                    .unwrap_or_default();
                (project.name.clone(), packages)
            })
            .collect(),
    )
}

/// Nx's `getTouchedProjectsFromTsConfig`: changed path mappings in the root
/// tsconfig touch the projects they point into; any other change touches all.
fn touched_by_root_tsconfig(
    workspace: &Workspace,
    changes: &Changes,
    touches: &mut Touches,
) -> Result<()> {
    let Some(file) = ["tsconfig.base.json", "tsconfig.json"]
        .into_iter()
        .find(|name| workspace.root.join(name).exists())
    else {
        return Ok(());
    };
    if !changes.changed(file) {
        return Ok(());
    }
    let everything = Reason::Tsconfig { file: file.into() };
    let FileChange::Json(diff) = changes.change(file) else {
        touches.all(workspace, everything);
        return Ok(());
    };
    let is_path_mapping = |change: &Change| {
        change.path[0] == "compilerOptions" && change.path.get(1).is_none_or(|key| key == "paths")
    };
    if !diff.iter().all(is_path_mapping) {
        touches.all(workspace, everything);
        return Ok(());
    }
    for change in diff.iter().filter(|change| change.path.len() == 4) {
        if change.kind == Kind::Deleted {
            touches.all(workspace, everything);
            return Ok(());
        }
        for value in [&change.before, &change.after].into_iter().flatten() {
            let Some(path) = value.as_str() else { continue };
            let path = path.strip_prefix("./").unwrap_or(path);
            for project in workspace.projects.values() {
                let root = project.root.strip_suffix('/').unwrap_or(&project.root);
                if !root.is_empty() && (path == root || path.starts_with(&format!("{root}/"))) {
                    touches.touch(
                        &project.name,
                        Reason::TsconfigPath {
                            file: file.into(),
                            mapping: change.path[2].clone(),
                            path: path.to_owned(),
                        },
                    );
                }
            }
        }
    }
    Ok(())
}

/// The package a snapshot key installs.
fn package_name(key: &str) -> &str {
    let key = &key[..key.find('(').unwrap_or(key.len())];
    let start = usize::from(key.starts_with('@'));
    key[start..].find('@').map_or(key, |at| &key[..start + at])
}
