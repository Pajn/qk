//! Deterministic workspace project graphs. External lockfile nodes come later.

mod npm_range;
pub use npm_range::satisfies;

/// Whether a dependency declared with `range` in the package at `root` means
/// the workspace package at `target`, as Nx decides it: a `workspace:`
/// range, `*`, a `file:` path to it, or a semver range its version
/// satisfies, prereleases included. Anything else, such as `catalog:`,
/// `npm:` aliases or a range the version is outside of, is left to npm.
fn links(range: &str, root: &str, target: &str, version: Option<&str>) -> bool {
    if range.starts_with("workspace:") || range == "*" {
        return true;
    }
    if let Some(path) = range.strip_prefix("file:")
        && join(root, path) == target
    {
        return true;
    }
    version.is_some_and(|version| satisfies(version, range))
}

/// Node's `path.posix.join`: segments joined and normalised, keeping a
/// trailing slash.
fn join(base: &str, path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    let mut up = 0;
    for part in base.split('/').chain(path.split('/')) {
        match part {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    up += 1;
                }
            }
            part => parts.push(part),
        }
    }
    let mut joined = vec![".."; up];
    joined.extend(parts);
    let mut result = if joined.is_empty() {
        ".".to_owned()
    } else {
        joined.join("/")
    };
    if path.ends_with('/') && result != "." {
        result.push('/');
    }
    result
}

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail};
use qk_config::{Project, Reader, Workspace, find_matching_projects};
use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct Node {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub data: Project,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Dependency {
    pub source: String,
    pub target: String,
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ProjectGraph {
    pub nodes: BTreeMap<String, Node>,
    pub dependencies: BTreeMap<String, Vec<Dependency>>,
}

#[derive(Serialize)]
pub struct GraphReport<'a> {
    pub graph: &'a ProjectGraph,
}

impl ProjectGraph {
    pub fn build(workspace: &Workspace) -> Result<Self> {
        // As in Nx, a package name two projects share means the later one,
        // in the order of their package.json paths.
        let mut packages: Vec<(String, &String)> = workspace
            .packages
            .keys()
            .map(|name| {
                let manifest = match workspace.projects[name].root.as_str() {
                    "." => "package.json".to_owned(),
                    root => format!("{root}/package.json"),
                };
                (manifest, name)
            })
            .collect();
        packages.sort();
        let mut package_names = BTreeMap::new();
        if workspace.analyzes_package_json() {
            for (_, project_name) in packages {
                if let Some(name) = &workspace.packages[project_name].name {
                    package_names.insert(name, project_name);
                }
            }
        }
        check_implicit_dependencies(&workspace.projects)?;
        let implicit: BTreeMap<&String, Vec<String>> = workspace
            .projects
            .iter()
            .map(|(name, project)| {
                (
                    name,
                    expand_implicit_dependencies(
                        name,
                        &project.implicit_dependencies,
                        &workspace.projects,
                    ),
                )
            })
            .collect();
        let mut graph = Self {
            nodes: BTreeMap::new(),
            dependencies: BTreeMap::new(),
        };
        for (name, project) in &workspace.projects {
            let mut edges = BTreeMap::new();
            if let Some(package) = workspace.packages.get(name) {
                for (dependency, range) in package.dependency_ranges() {
                    if let Some(target) = package_names.get(&dependency.to_owned())
                        && *target != name
                        && links(
                            range,
                            &project.root,
                            &workspace.projects[*target].root,
                            workspace.packages[*target].version.as_deref(),
                        )
                    {
                        edges
                            .entry((*target).clone())
                            .or_insert_with(BTreeSet::new)
                            .insert("static");
                    }
                }
            }
            for target in &implicit[name] {
                match target.strip_prefix('!') {
                    Some(target) => {
                        edges.remove(target);
                    }
                    // Nx keeps an implicit edge alongside a static one for the same pair.
                    None => {
                        edges
                            .entry(target.clone())
                            .or_insert_with(BTreeSet::new)
                            .insert("implicit");
                    }
                }
            }
            graph.dependencies.insert(
                name.clone(),
                edges
                    .into_iter()
                    .flat_map(|(target, kinds)| {
                        kinds.into_iter().map(move |kind| Dependency {
                            source: name.clone(),
                            target: target.clone(),
                            kind: kind.into(),
                        })
                    })
                    .collect(),
            );
            let mut data = project.clone();
            data.implicit_dependencies = implicit[name].clone();
            graph.nodes.insert(
                name.clone(),
                Node {
                    name: name.clone(),
                    kind: node_type(workspace, name, project).into(),
                    data,
                },
            );
        }
        Ok(graph)
    }

    /// Return transitive dependents, including the original projects.
    pub fn dependents_of(&self, projects: &BTreeSet<String>) -> Result<BTreeSet<String>> {
        let mut reverse: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for edges in self.dependencies.values() {
            for edge in edges {
                reverse.entry(&edge.target).or_default().push(&edge.source);
            }
        }
        let mut visited = projects.clone();
        let mut pending: Vec<String> = projects.iter().cloned().collect();
        while let Some(name) = pending.pop() {
            if !self.nodes.contains_key(&name) {
                bail!("unknown project {name:?}");
            }
            for dependent in reverse.get(name.as_str()).into_iter().flatten() {
                if visited.insert((*dependent).to_owned()) {
                    pending.push((*dependent).to_owned());
                }
            }
        }
        Ok(visited)
    }

    /// The project, the projects it depends on and the projects depending on
    /// it, transitively: the projects `nx graph --focus` keeps.
    pub fn related_to(&self, project: &str) -> Result<BTreeSet<String>> {
        let mut related = self.dependents_of(&BTreeSet::from([project.to_owned()]))?;
        let mut visited = BTreeSet::from([project]);
        let mut pending = vec![project];
        while let Some(name) = pending.pop() {
            for edge in self.dependencies.get(name).into_iter().flatten() {
                if visited.insert(&edge.target) {
                    related.insert(edge.target.clone());
                    pending.push(&edge.target);
                }
            }
        }
        Ok(related)
    }

    /// Keeps only the named projects. As in Nx, the kept projects' edges stay
    /// as they are, including those to projects left out.
    pub fn retain(&mut self, projects: &BTreeSet<String>) {
        self.nodes.retain(|name, _| projects.contains(name));
        self.dependencies.retain(|name, _| projects.contains(name));
    }
}

/// A node's `type`, as Nx's `getProjectType` derives it.
fn node_type(workspace: &Workspace, name: &str, project: &Project) -> &'static str {
    if let Some(project_type) = &project.project_type {
        return if project_type == "library" {
            "lib"
        } else if name.ends_with("-e2e") || name == "e2e" {
            "e2e"
        } else {
            "app"
        };
    }
    let root = workspace.root.join(&project.root);
    if root.join("tsconfig.lib.json").exists() {
        return "lib";
    }
    if root.join("tsconfig.app.json").exists() {
        return "app";
    }
    match workspace.packages.get(name) {
        Some(package)
            if package.exports.is_none()
                && package.main.is_none()
                && package.module.is_none()
                && package.bin.is_none() =>
        {
            "app"
        }
        // Nx assumes an application only when it can read an entry-point-free package.
        Some(_) => "lib",
        None => "lib",
    }
}

/// Orders project names as Nx orders its graph nodes: by name, then stably by
/// descending root length (`normalizeProjectNodes` in Nx).
pub fn graph_order(
    projects: &BTreeMap<String, Project>,
    names: impl IntoIterator<Item = String>,
) -> Vec<String> {
    let mut names: Vec<_> = names.into_iter().collect();
    names.sort();
    names.sort_by_key(|name| std::cmp::Reverse(projects[name].root.len()));
    names
}

/// The projects `--projects` and `--exclude` select, as Nx's `run-many`
/// selects them: every project without patterns, else what the patterns
/// match, in order, less what the exclusions match among those.
pub fn select_projects(
    projects: &BTreeMap<String, Project>,
    selectors: &[String],
    excludes: &[String],
) -> Result<BTreeSet<String>> {
    let selected: BTreeMap<String, &Project> = if selectors.is_empty() {
        projects
            .iter()
            .map(|(name, project)| (name.clone(), project))
            .collect()
    } else {
        find_matching_projects(selectors, projects, Reader::Nx)
            .into_iter()
            .map(|name| {
                let project = &projects[&name];
                (name, project)
            })
            .collect()
    };
    let excluded: BTreeSet<String> = find_matching_projects(excludes, &selected, Reader::Nx)
        .into_iter()
        .collect();
    Ok(selected
        .into_keys()
        .filter(|name| !excluded.contains(name))
        .collect())
}

/// Nx's `assertWorkspaceValidity`: every implicit dependency, `!` ones
/// included, names a project or a pattern matching one. `nx-cloud:`
/// references to other workspaces are not checked.
fn check_implicit_dependencies(projects: &BTreeMap<String, Project>) -> Result<()> {
    let mut invalid = Vec::new();
    for (name, project) in projects {
        let missing: Vec<&String> = project
            .implicit_dependencies
            .iter()
            .filter(|dependency| {
                let target = dependency.strip_prefix('!').unwrap_or(dependency);
                !target.starts_with("nx-cloud:")
                    && !projects.contains_key(target)
                    && find_matching_projects(&[dependency.as_str()], projects, Reader::Nx)
                        .is_empty()
            })
            .collect();
        if !missing.is_empty() {
            let mut message = format!("  {name}");
            for dependency in missing {
                message.push_str(&format!("\n    {dependency}"));
            }
            invalid.push(message);
        }
    }
    if !invalid.is_empty() {
        bail!(
            "The following implicitDependencies point to non-existent project(s):\n{}",
            invalid.join("\n\n")
        );
    }
    Ok(())
}

/// Nx's `normalizeImplicitDependencies`: the projects the patterns select,
/// the project itself left out, then a `!` and the name of each project an
/// exclusion matches, which removes any other edge to it.
fn expand_implicit_dependencies(
    source: &str,
    dependencies: &[String],
    projects: &BTreeMap<String, Project>,
) -> Vec<String> {
    let (negative, positive): (Vec<&String>, Vec<&String>) = dependencies
        .iter()
        .partition(|dependency| dependency.starts_with('!'));
    let mut expanded: Vec<String> = if positive.is_empty() {
        Vec::new()
    } else {
        let patterns: Vec<&String> = positive.iter().chain(&negative).copied().collect();
        find_matching_projects(&patterns, projects, Reader::Nx)
            .into_iter()
            .filter(|name| name != source)
            .collect()
    };
    let negative: Vec<&str> = negative.iter().map(|dependency| &dependency[1..]).collect();
    expanded.extend(
        find_matching_projects(&negative, projects, Reader::Nx)
            .into_iter()
            .map(|name| format!("!{name}")),
    );
    expanded
}
