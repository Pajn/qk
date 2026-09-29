//! Deterministic workspace project graphs. External lockfile nodes come later.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};
use globset::Glob;
use qk_config::{Project, Workspace};
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
        let mut package_names = BTreeMap::new();
        for (project_name, package) in &workspace.packages {
            if let Some(name) = &package.name
                && let Some(previous) = package_names.insert(name, project_name)
            {
                bail!(
                    "duplicate workspace package name {name:?} in projects {previous:?} and {project_name:?}"
                );
            }
        }
        let mut graph = Self {
            nodes: BTreeMap::new(),
            dependencies: BTreeMap::new(),
        };
        for (name, project) in &workspace.projects {
            let mut edges = BTreeMap::new();
            if let Some(package) = workspace.packages.get(name) {
                for dependency in package.dependency_names() {
                    if let Some(target) = package_names.get(dependency)
                        && *target != name
                    {
                        edges.insert((*target).clone(), "static");
                    }
                }
            }
            for selector in project
                .implicit_dependencies
                .iter()
                .filter(|s| !s.starts_with('!'))
            {
                let selected = matches(&workspace.projects, selector)
                    .with_context(|| format!("implicitDependencies of project {name:?}"))?;
                if selected.is_empty() && !is_pattern(selector) {
                    bail!("project {name:?} has unknown implicit dependency {selector:?}");
                }
                for target in selected {
                    if target != *name {
                        edges.entry(target).or_insert("implicit");
                    }
                }
            }
            for selector in project
                .implicit_dependencies
                .iter()
                .filter_map(|s| s.strip_prefix('!'))
            {
                for target in matches(&workspace.projects, selector)? {
                    edges.remove(&target);
                }
            }
            graph.dependencies.insert(
                name.clone(),
                edges
                    .into_iter()
                    .map(|(target, kind)| Dependency {
                        source: name.clone(),
                        target,
                        kind: kind.into(),
                    })
                    .collect(),
            );
            graph.nodes.insert(
                name.clone(),
                Node {
                    name: name.clone(),
                    kind: if project.project_type.as_deref() == Some("application") {
                        "app"
                    } else {
                        "lib"
                    }
                    .into(),
                    data: project.clone(),
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
}

/// Positive selectors are unioned; exclusions win regardless of their order.
/// With no positive selectors, start with all projects.
pub fn select_projects(
    projects: &BTreeMap<String, Project>,
    selectors: &[String],
    excludes: &[String],
) -> Result<BTreeSet<String>> {
    let positives: Vec<_> = selectors.iter().filter(|s| !s.starts_with('!')).collect();
    let mut selected = if positives.is_empty() {
        projects.keys().cloned().collect()
    } else {
        BTreeSet::new()
    };
    for selector in positives {
        selected.extend(matches(projects, selector)?);
    }
    for selector in selectors
        .iter()
        .filter_map(|s| s.strip_prefix('!'))
        .chain(excludes.iter().map(String::as_str))
    {
        for name in matches(projects, selector)? {
            selected.remove(&name);
        }
    }
    Ok(selected)
}

fn matches(projects: &BTreeMap<String, Project>, selector: &str) -> Result<BTreeSet<String>> {
    if selector.is_empty() {
        bail!("project selector must not be empty");
    }
    let tag = selector.strip_prefix("tag:");
    let glob = Glob::new(tag.unwrap_or(selector))
        .with_context(|| format!("invalid project selector {selector:?}"))?
        .compile_matcher();
    Ok(projects
        .iter()
        .filter(|(name, project)| {
            if tag.is_some() {
                project.tags.iter().any(|tag| glob.is_match(tag))
            } else {
                glob.is_match(name.as_str())
            }
        })
        .map(|(name, _)| name.clone())
        .collect())
}

fn is_pattern(selector: &str) -> bool {
    selector.starts_with("tag:") || selector.contains(['*', '?', '[', '{'])
}
