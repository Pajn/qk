//! Which projects a set of changed files affects, following Nx 23's
//! `filterAffected`: files are attributed to projects by the locators below,
//! then every project depending on a touched one is affected too.
//!
//! One locator is deliberately more precise than Nx. For a `pnpm-lock.yaml`
//! change under `projectsAffectedByDependencyUpdates: "auto"`, a project is
//! touched when what its importer installs differs between the revisions.
//! Nx diffs versions by package name, so it also reports projects that install
//! exactly what they did before.

mod json_diff;
mod projections;
mod reachability;
mod subprocess;
mod tasks;

pub use projections::{ProjectionFallbackKind, ProjectionFallbackPolicy, ProjectionReport};
pub use reachability::{Decision, Kept};
pub use tasks::{TaskAnalysis, TaskCause, TaskReason, affected_tasks};

use std::cell::OnceCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use subprocess::workspace_command;

use anyhow::{Context, Result, bail, ensure};
use qk_cache::Pattern;
use qk_config::Workspace;
use qk_graph::{ProjectGraph, select_projects};
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

const LOCKFILES: &[&str] = &[
    "pnpm-lock.yaml",
    "package-lock.json",
    "yarn.lock",
    "bun.lock",
    "bun.lockb",
];

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
    /// Under a profile with reachability, what it decided for each project
    /// with `qk:reachability` affected only through dependencies, and for
    /// each project affected only through those it left out.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub reachability: BTreeMap<String, Decision>,
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
    /// `nx.json` changed, which touches every project.
    NxJson,
    /// A changed file matches a `{workspaceRoot}` input of one of its targets.
    WorkspaceInput {
        file: String,
        target: String,
        input: String,
    },
    /// A project manifest was deleted, which touches every project.
    DeletedManifest { file: String },
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
    /// A dependency in the root `package.json` changed, and the project
    /// installs that package or is it.
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
            Self::NxJson => write!(f, "nx.json changed, which affects every project"),
            Self::WorkspaceInput {
                file,
                target,
                input,
            } => write!(
                f,
                "{file} changed, matching input {input} of target {target}"
            ),
            Self::DeletedManifest { file } => write!(
                f,
                "{file} was deleted, which affects every project because its dependents are unknown"
            ),
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

pub fn analyse(workspace: &Workspace, graph: &ProjectGraph, options: &Options) -> Result<Analysis> {
    let changes = Changes::new(workspace, options)?;
    let mut touches = Touches::default();
    for locator in [
        touched_by_path,
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
    if let Some(profile) = &options.affected_profile
        && projections::reachability(workspace, profile)?
    {
        reachability = reachability::decide(workspace, graph, &changes, &projects)?;
        let left_out: BTreeSet<String> = reachability
            .iter()
            .filter(|(_, decision)| matches!(decision, Decision::LeftOut { .. }))
            .map(|(name, _)| name.clone())
            .collect();
        if !left_out.is_empty() {
            let narrowed = propagate(graph, &touches.0, &left_out);
            for (name, cause) in &projects {
                if narrowed.contains_key(name) || left_out.contains(name) {
                    continue;
                }
                if let Cause::DependsOn { project, .. } = cause {
                    reachability.insert(
                        name.clone(),
                        Decision::Through {
                            project: project.clone(),
                        },
                    );
                }
            }
            projects = narrowed;
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
        while let Some(Cause::DependsOn { project, .. }) = self.projects.get(*chain.last().unwrap())
        {
            chain.push(project);
        }
        Some(chain)
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
    files: Vec<String>,
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
        let head_revision = head.as_deref().unwrap_or("HEAD");
        let (merged, upstream) = merge_base(root, &requested, head_revision);
        let range = (options.files.is_empty()
            && !options.explicit_files
            && !options.uncommitted
            && !options.untracked)
            .then(|| {
                let default_branch = upstream_of(root, &default_base).or_else(|| {
                    git_lines(root, &["rev-parse", "--verify", "--quiet", &default_base])
                        .ok()
                        .map(|_| default_base.clone())
                });
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
        let files = if options.explicit_files || !options.files.is_empty() {
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
            files,
            base,
            head,
            range,
        })
    }

    fn changed(&self, file: &str) -> bool {
        self.files.iter().any(|changed| changed == file)
    }

    /// Nx's `calculateFileChanges` for one file.
    fn change(&self, file: &str) -> FileChange {
        if !self.workspace.root.join(file).exists() {
            return FileChange::Deleted;
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

/// The paths a Git command lists, read NUL-separated so that Git does not
/// quote names outside ASCII. Nx reads them line by line, quoted, and so
/// misses a change to such a file.
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

/// Each file touches the project whose root most specifically contains it.
fn touched_by_path(workspace: &Workspace, changes: &Changes, touches: &mut Touches) -> Result<()> {
    let roots: BTreeMap<&str, &str> = workspace
        .projects
        .values()
        .map(|project| (project.root.as_str(), project.name.as_str()))
        .collect();
    for file in &changes.files {
        let mut path = file.as_str();
        loop {
            if let Some(project) = roots.get(path) {
                touches.touch(project, Reason::File { file: file.clone() });
                break;
            }
            match path.rfind('/') {
                Some(slash) => path = &path[..slash],
                None if path != "." => path = ".",
                None => break,
            }
        }
    }
    Ok(())
}

/// `nx.json`, or the file it extends, touches every project; a file named by
/// a `{workspaceRoot}` input touches the projects declaring it.
fn touched_implicitly(
    workspace: &Workspace,
    changes: &Changes,
    touches: &mut Touches,
) -> Result<()> {
    if changes.changed("nx.json")
        || workspace
            .extended
            .as_ref()
            .is_some_and(|extended| changes.changed(extended))
    {
        touches.all(workspace, Reason::NxJson);
        return Ok(());
    }
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
    let mut workspace_file_reaches_tasks = None;
    for (pattern, declarations) in patterns {
        let matcher = Pattern::new(&pattern, false)?;
        let Some(file) = changes.files.iter().find(|file| {
            matcher.is_match(file)
                && (*file != "pnpm-workspace.yaml"
                    || *workspace_file_reaches_tasks
                        .get_or_insert_with(|| pnpm_workspace_change_reaches_tasks(changes)))
        }) else {
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

fn pnpm_workspace_change_reaches_tasks(changes: &Changes) -> bool {
    match changes.change("pnpm-workspace.yaml") {
        FileChange::Json(changes) => changes
            .iter()
            .any(|change| !qk_lockfile::RESOLUTION_KEYS.contains(&change.path[0].as_str())),
        _ => true,
    }
}

/// A deleted project manifest may have removed a project, which leaves Nx no
/// way of knowing what depended on it.
fn touched_by_deleted_manifests(
    workspace: &Workspace,
    changes: &Changes,
    touches: &mut Touches,
) -> Result<()> {
    let deleted = changes.files.iter().find(|file| {
        let name = file.rsplit('/').next().unwrap_or(file);
        matches!(name, "project.json" | "package.json") && !workspace.root.join(file).exists()
    });
    if let Some(file) = deleted {
        touches.all(workspace, Reason::DeletedManifest { file: file.clone() });
    }
    Ok(())
}

fn touched_by_lockfile(
    workspace: &Workspace,
    changes: &Changes,
    touches: &mut Touches,
) -> Result<()> {
    let Some(file) = changes.files.iter().find(|file| {
        [
            "pnpm-lock.yaml",
            "package-lock.json",
            "yarn.lock",
            "bun.lock",
        ]
        .contains(&file.as_str())
    }) else {
        return Ok(());
    };
    let setting = projects_affected_by_dependency_updates(workspace);
    let reason = Reason::DependencyUpdates {
        file: file.clone(),
        setting: setting.clone(),
    };
    match &setting {
        Value::String(mode) if mode == "auto" => {}
        Value::Array(selectors) => {
            let selectors: Vec<String> = serde_json::from_value(Value::Array(selectors.clone()))
                .context("projectsAffectedByDependencyUpdates must list project selectors")?;
            for project in select_projects(&workspace.projects, &selectors, &[])? {
                touches.touch(&project, reason.clone());
            }
            return Ok(());
        }
        _ => {
            touches.all(workspace, reason);
            return Ok(());
        }
    }
    let unreadable = |detail: &str| Reason::UnreadableLockfile {
        file: file.clone(),
        detail: detail.to_owned(),
    };
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

/// `pluginsConfig["@nx/js"].projectsAffectedByDependencyUpdates`, which Nx
/// defaults to `"all"`.
fn projects_affected_by_dependency_updates(workspace: &Workspace) -> Value {
    workspace
        .config
        .extra
        .get("pluginsConfig")
        .and_then(|config| config.get("@nx/js"))
        .and_then(|config| config.get("projectsAffectedByDependencyUpdates"))
        .cloned()
        .unwrap_or_else(|| Value::String("all".into()))
}

/// Nx's `getTouchedNpmPackages`: a dependency changed in the root
/// `package.json` touches the projects installing that package.
fn touched_by_root_package(
    workspace: &Workspace,
    changes: &Changes,
    touches: &mut Touches,
) -> Result<()> {
    if !changes.changed("package.json") {
        return Ok(());
    }
    let installed = installed_packages(workspace, changes);
    let installing = |name: &str| -> Option<BTreeSet<String>> {
        let installed = installed.as_ref()?;
        let projects: BTreeSet<String> = installed
            .iter()
            .filter(|(_, packages)| packages.contains(name))
            .map(|(project, _)| project.clone())
            .collect();
        (!projects.is_empty()).then_some(projects)
    };
    let FileChange::Json(diff) = changes.change("package.json") else {
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
    };
    for change in diff {
        let path: Vec<&str> = change.path.iter().map(String::as_str).collect();
        let dotted = path.join(".");
        let everything = |detail: String, touches: &mut Touches| {
            touches.all(workspace, Reason::RootPackage { detail });
        };
        match path.as_slice() {
            ["dependencies" | "devDependencies", name] => {
                if change.kind == Kind::Deleted {
                    everything(format!("removed {dotted}"), touches);
                    return Ok(());
                }
                if *name == "nx" {
                    everything(
                        format!("changed {dotted}, which Nx treats as global"),
                        touches,
                    );
                    return Ok(());
                }
                let mut touch = |projects: BTreeSet<String>, name: &str| {
                    for project in projects {
                        touches.touch(
                            &project,
                            Reason::RootDependency {
                                name: name.to_owned(),
                                path: dotted.clone(),
                            },
                        );
                    }
                };
                if let Some(projects) = installing(name) {
                    touch(projects, name);
                    if let Some(implementation) = name.strip_prefix("@types/")
                        && let Some(projects) = installing(implementation)
                    {
                        touch(projects, implementation);
                    }
                } else if workspace.projects.contains_key(*name) {
                    touch(BTreeSet::from([(*name).to_owned()]), name);
                }
            }
            ["overrides" | "resolutions", name, ..] | ["pnpm", "overrides", name, ..] => {
                if *name == "nx" {
                    everything(
                        format!("changed {dotted}, which Nx treats as global"),
                        touches,
                    );
                    return Ok(());
                }
                match installing(name) {
                    Some(projects) => {
                        for project in projects {
                            touches.touch(
                                &project,
                                Reason::RootDependency {
                                    name: (*name).to_owned(),
                                    path: dotted.clone(),
                                },
                            );
                        }
                    }
                    None => {
                        everything(
                            format!("changed {dotted} for a package nothing installs"),
                            touches,
                        );
                        return Ok(());
                    }
                }
            }
            _ => {}
        }
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
