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

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use qk_cache::Pattern;
use qk_config::Workspace;
use qk_graph::{ProjectGraph, select_projects};
use qk_lockfile::Lockfile;
use serde_json::Value;

use json_diff::{Change, Kind};

/// What to compare, as Nx's affected options spell it.
#[derive(Clone, Debug, Default)]
pub struct Options {
    pub base: Option<String>,
    pub head: Option<String>,
    pub files: Vec<String>,
    pub uncommitted: bool,
    pub untracked: bool,
}

/// Keys of `pnpm-workspace.yaml` that configure resolution. A change confined
/// to them reaches tasks only through `pnpm-lock.yaml`.
const PNPM_RESOLUTION_KEYS: &[&str] = &[
    "catalog",
    "catalogs",
    "dedupePeerDependents",
    "dedupePeers",
    "minimumReleaseAge",
    "minimumReleaseAgeExclude",
    "overrides",
    "patchedDependencies",
    "peerDependencyRules",
    "resolutionMode",
];

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
    let changes = Changes::new(workspace, options)?;
    let mut touched = BTreeSet::new();
    for locator in [
        touched_by_path,
        touched_implicitly,
        touched_by_deleted_manifests,
        touched_by_lockfile,
        touched_by_root_package,
        touched_by_root_tsconfig,
    ] {
        touched.extend(locator(workspace, graph, &changes)?);
    }
    // Dependents of a touched project are affected, transitively.
    let mut dependents: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (source, edges) in &graph.dependencies {
        for edge in edges {
            dependents.entry(&edge.target).or_default().push(source);
        }
    }
    let mut affected = BTreeSet::new();
    let mut pending: Vec<String> = touched.into_iter().collect();
    while let Some(project) = pending.pop() {
        if !workspace.projects.contains_key(&project) {
            bail!("invalid project name {project:?}");
        }
        if affected.insert(project.clone()) {
            for dependent in dependents.get(project.as_str()).into_iter().flatten() {
                pending.push((*dependent).to_owned());
            }
        }
    }
    Ok(affected)
}

/// The changed files, and each one's content at the two revisions.
struct Changes<'a> {
    workspace: &'a Workspace,
    files: Vec<String>,
    base: Option<String>,
    /// `None` reads the working tree.
    head: Option<String>,
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
        let root = &workspace.root;
        let mut base = options.base.clone().or_else(|| non_empty_env("NX_BASE"));
        let head = options.head.clone().or_else(|| non_empty_env("NX_HEAD"));
        if base.is_none() {
            base = Some(
                workspace
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
                    .unwrap_or_else(|| "main".into()),
            );
        }
        let base = base.map(|base| merge_base(root, &base, head.as_deref().unwrap_or("HEAD")));
        let files = if !options.files.is_empty() {
            options.files.clone()
        } else if options.uncommitted {
            uncommitted(root)?
        } else if options.untracked {
            untracked(root)?
        } else if let (Some(base), Some(head)) = (&base, &head) {
            git_lines(
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
            let mut files: BTreeSet<String> = git_lines(
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
        let mut ignore = ignore::gitignore::GitignoreBuilder::new(root);
        for name in [".gitignore", ".nxignore"] {
            if root.join(name).is_file() {
                ignore.add(root.join(name));
            }
        }
        let ignore = ignore.build()?;
        let files = files
            .into_iter()
            .filter(|file| !ignore.matched_path_or_any_parents(file, false).is_ignore())
            .collect();
        Ok(Self {
            workspace,
            files,
            base,
            head,
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

    fn read(&self, file: &str, revision: Option<&str>) -> Option<String> {
        match revision {
            None => std::fs::read_to_string(self.workspace.root.join(file)).ok(),
            Some(revision) => {
                let output = Command::new("git")
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

fn merge_base(root: &Path, base: &str, head: &str) -> String {
    for args in [
        vec!["merge-base", base, head],
        vec!["merge-base", "--fork-point", base, head],
    ] {
        if let Ok(lines) = git_lines(root, &args)
            && let Some(sha) = lines.into_iter().next()
        {
            return sha;
        }
    }
    base.to_owned()
}

fn uncommitted(root: &Path) -> Result<Vec<String>> {
    git_lines(
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

fn untracked(root: &Path) -> Result<Vec<String>> {
    git_lines(root, &["ls-files", "--others", "--exclude-standard"])
}

fn git_lines(root: &Path, args: &[&str]) -> Result<Vec<String>> {
    let output = Command::new("git")
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

fn all(workspace: &Workspace) -> BTreeSet<String> {
    workspace.projects.keys().cloned().collect()
}

type Touched = Result<BTreeSet<String>>;

/// Each file touches the project whose root most specifically contains it.
fn touched_by_path(workspace: &Workspace, _: &ProjectGraph, changes: &Changes) -> Touched {
    let roots: BTreeMap<&str, &str> = workspace
        .projects
        .values()
        .map(|project| (project.root.as_str(), project.name.as_str()))
        .collect();
    let mut touched = BTreeSet::new();
    for file in &changes.files {
        let mut path = file.as_str();
        loop {
            if let Some(project) = roots.get(path) {
                touched.insert((*project).to_owned());
                break;
            }
            match path.rfind('/') {
                Some(slash) => path = &path[..slash],
                None if path != "." => path = ".",
                None => break,
            }
        }
    }
    Ok(touched)
}

/// `nx.json` touches every project; a file named by a `{workspaceRoot}` input
/// touches the projects declaring it.
fn touched_implicitly(workspace: &Workspace, _: &ProjectGraph, changes: &Changes) -> Touched {
    if changes.changed("nx.json") {
        return Ok(all(workspace));
    }
    let mut patterns: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for project in workspace.projects.values() {
        for target in project.targets.values() {
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
                    .insert(project.name.clone());
            }
        }
    }
    let mut workspace_file_reaches_tasks = None;
    let mut touched = BTreeSet::new();
    for (pattern, projects) in patterns {
        let matcher = Pattern::new(&pattern, false)?;
        let changed = changes.files.iter().any(|file| {
            matcher.is_match(file)
                && (file != "pnpm-workspace.yaml"
                    || *workspace_file_reaches_tasks
                        .get_or_insert_with(|| pnpm_workspace_change_reaches_tasks(changes)))
        });
        if changed {
            touched.extend(projects);
        }
    }
    Ok(touched)
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
            .any(|change| !PNPM_RESOLUTION_KEYS.contains(&change.path[0].as_str())),
        _ => true,
    }
}

/// A deleted project manifest may have removed a project, which leaves Nx no
/// way of knowing what depended on it.
fn touched_by_deleted_manifests(
    workspace: &Workspace,
    _: &ProjectGraph,
    changes: &Changes,
) -> Touched {
    let deleted = changes.files.iter().any(|file| {
        let name = file.rsplit('/').next().unwrap_or(file);
        matches!(name, "project.json" | "package.json") && !workspace.root.join(file).exists()
    });
    Ok(if deleted {
        all(workspace)
    } else {
        BTreeSet::new()
    })
}

fn touched_by_lockfile(workspace: &Workspace, _: &ProjectGraph, changes: &Changes) -> Touched {
    let Some(file) = changes.files.iter().find(|file| {
        [
            "pnpm-lock.yaml",
            "package-lock.json",
            "yarn.lock",
            "bun.lock",
        ]
        .contains(&file.as_str())
    }) else {
        return Ok(BTreeSet::new());
    };
    match projects_affected_by_dependency_updates(workspace) {
        Value::String(mode) if mode == "auto" => {}
        Value::Array(selectors) => {
            let selectors: Vec<String> = serde_json::from_value(Value::Array(selectors))
                .context("projectsAffectedByDependencyUpdates must list project selectors")?;
            return Ok(select_projects(&workspace.projects, &selectors, &[])?
                .into_iter()
                .collect());
        }
        _ => return Ok(all(workspace)),
    }
    let FileChange::Lockfile { before, after } = changes.change(file) else {
        return Ok(all(workspace));
    };
    if file != "pnpm-lock.yaml" {
        // qk reads pnpm lockfiles only.
        return Ok(all(workspace));
    }
    let (Ok(before), Ok(after)) = (Lockfile::parse(&before), Lockfile::parse(&after)) else {
        return Ok(all(workspace));
    };
    Ok(workspace
        .projects
        .values()
        .filter(|project| before.installed(&project.root) != after.installed(&project.root))
        .map(|project| project.name.clone())
        .collect())
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
fn touched_by_root_package(workspace: &Workspace, _: &ProjectGraph, changes: &Changes) -> Touched {
    if !changes.changed("package.json") {
        return Ok(BTreeSet::new());
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
        return Ok(match &installed {
            Some(installed) => installed
                .iter()
                .filter(|(_, packages)| !packages.is_empty())
                .map(|(project, _)| project.clone())
                .collect(),
            None => all(workspace),
        });
    };
    let mut touched = BTreeSet::new();
    for change in diff {
        let path: Vec<&str> = change.path.iter().map(String::as_str).collect();
        match path.as_slice() {
            ["dependencies" | "devDependencies", name] => {
                if change.kind == Kind::Deleted {
                    return Ok(all(workspace));
                }
                if *name == "nx" {
                    return Ok(all(workspace));
                }
                if let Some(projects) = installing(name) {
                    touched.extend(projects);
                    if let Some(implementation) = name.strip_prefix("@types/")
                        && let Some(projects) = installing(implementation)
                    {
                        touched.extend(projects);
                    }
                } else if workspace.projects.contains_key(*name) {
                    touched.insert((*name).to_owned());
                }
            }
            ["overrides" | "resolutions", name, ..] | ["pnpm", "overrides", name, ..] => {
                if *name == "nx" {
                    return Ok(all(workspace));
                }
                match installing(name) {
                    Some(projects) => touched.extend(projects),
                    None => return Ok(all(workspace)),
                }
            }
            _ => {}
        }
    }
    Ok(touched)
}

/// The package names each project's importer installs, from the head revision
/// of `pnpm-lock.yaml`, or `None` without a lockfile qk can read.
fn installed_packages(
    workspace: &Workspace,
    changes: &Changes,
) -> Option<BTreeMap<String, BTreeSet<String>>> {
    let text = changes.read("pnpm-lock.yaml", changes.head.as_deref())?;
    let lockfile = Lockfile::parse(&text).ok()?;
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
fn touched_by_root_tsconfig(workspace: &Workspace, _: &ProjectGraph, changes: &Changes) -> Touched {
    let Some(file) = ["tsconfig.base.json", "tsconfig.json"]
        .into_iter()
        .find(|name| workspace.root.join(name).exists())
    else {
        return Ok(BTreeSet::new());
    };
    if !changes.changed(file) {
        return Ok(BTreeSet::new());
    }
    let FileChange::Json(diff) = changes.change(file) else {
        return Ok(all(workspace));
    };
    let is_path_mapping = |change: &Change| {
        change.path[0] == "compilerOptions" && change.path.get(1).is_none_or(|key| key == "paths")
    };
    if !diff.iter().all(is_path_mapping) {
        return Ok(all(workspace));
    }
    let mut touched = BTreeSet::new();
    for change in diff.iter().filter(|change| change.path.len() == 4) {
        if change.kind == Kind::Deleted {
            return Ok(all(workspace));
        }
        for value in [&change.before, &change.after].into_iter().flatten() {
            let Some(path) = value.as_str() else { continue };
            let path = path.strip_prefix("./").unwrap_or(path);
            for project in workspace.projects.values() {
                let root = project.root.strip_suffix('/').unwrap_or(&project.root);
                if !root.is_empty() && (path == root || path.starts_with(&format!("{root}/"))) {
                    touched.insert(project.name.clone());
                }
            }
        }
    }
    Ok(touched)
}
