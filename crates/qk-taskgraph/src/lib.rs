//! Expand requested targets into a validated task DAG before executing anything.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};
use qk_config::{Target, Workspace};
use qk_graph::{ProjectGraph, select_projects};
use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Debug)]
pub struct Request {
    pub project: String,
    pub target: String,
    pub configuration: Option<String>,
    pub args: Vec<String>,
}

impl Request {
    pub fn parse(value: &str) -> Result<Self> {
        let parts: Vec<_> = value.split(':').collect();
        if !(2..=3).contains(&parts.len()) || parts.iter().any(|part| part.is_empty()) {
            bail!("expected project:target[:configuration], got {value:?}");
        }
        Ok(Self {
            project: parts[0].into(),
            target: parts[1].into(),
            configuration: parts.get(2).map(|part| (*part).into()),
            args: Vec::new(),
        })
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Task {
    pub id: String,
    pub project: String,
    pub target: String,
    pub configuration: Option<String>,
    pub args: Vec<String>,
    pub definition: Target,
    pub dependencies: BTreeSet<String>,
}

#[derive(Debug, Serialize)]
pub struct TaskGraph {
    pub roots: BTreeSet<String>,
    pub tasks: BTreeMap<String, Task>,
}

impl TaskGraph {
    pub fn build(workspace: &Workspace, requests: &[Request]) -> Result<Self> {
        if requests.is_empty() {
            bail!("no tasks selected");
        }
        let projects = ProjectGraph::build(workspace)?;
        let mut builder = Builder {
            workspace,
            projects,
            tasks: BTreeMap::new(),
            visiting: Vec::new(),
        };
        let mut roots = BTreeSet::new();
        for request in requests {
            roots.insert(builder.visit(request.clone(), true)?.with_context(|| {
                format!(
                    "project {:?} has no target {:?}",
                    request.project, request.target
                )
            })?);
        }
        Ok(Self {
            roots,
            tasks: builder.tasks,
        })
    }
}

struct Builder<'a> {
    workspace: &'a Workspace,
    projects: ProjectGraph,
    tasks: BTreeMap<String, Task>,
    visiting: Vec<String>,
}

impl Builder<'_> {
    fn visit(&mut self, request: Request, required: bool) -> Result<Option<String>> {
        let project = self
            .workspace
            .projects
            .get(&request.project)
            .with_context(|| format!("unknown project {:?}", request.project))?;
        let Some(target) = project.targets.get(&request.target) else {
            if required {
                bail!(
                    "project {:?} has no target {:?}",
                    request.project,
                    request.target
                );
            }
            return Ok(None);
        };
        let configuration = match request.configuration {
            Some(name) if target.configurations.contains_key(&name) => Some(name),
            Some(name) if required => bail!(
                "{}:{} has no configuration {name:?}",
                request.project,
                request.target
            ),
            _ => target.default_configuration.clone(),
        };
        let id = format!(
            "{}:{}{}",
            request.project,
            request.target,
            configuration
                .as_ref()
                .map(|c| format!(":{c}"))
                .unwrap_or_default()
        );
        if let Some(position) = self.visiting.iter().position(|task| task == &id) {
            let mut cycle = self.visiting[position..].to_vec();
            cycle.push(id);
            bail!("task dependency cycle: {}", cycle.join(" -> "));
        }
        if let Some(existing) = self.tasks.get(&id) {
            if existing.args != request.args {
                bail!("task {id} requested with conflicting forwarded arguments");
            }
            return Ok(Some(id));
        }
        let mut definition = target.clone();
        if let Some(name) = &configuration {
            let options = target
                .configurations
                .get(name)
                .with_context(|| format!("{id}: default configuration does not exist"))?;
            for (key, value) in options {
                if key == "outputs" {
                    definition.outputs =
                        Some(serde_json::from_value(value.clone()).with_context(|| {
                            format!("{id}: configuration outputs must be an array of paths")
                        })?);
                } else if key == "env" {
                    let overlay = value
                        .as_object()
                        .with_context(|| format!("{id}: configuration env must be an object"))?;
                    let base = definition
                        .options
                        .entry("env")
                        .or_insert_with(|| serde_json::json!({}));
                    base.as_object_mut()
                        .with_context(|| format!("{id}: options.env must be an object"))?
                        .extend(overlay.clone());
                } else {
                    definition.options.insert(key.clone(), value.clone());
                }
            }
        }
        let task = Task {
            id: id.clone(),
            project: request.project,
            target: request.target,
            configuration,
            args: request.args,
            definition,
            dependencies: BTreeSet::new(),
        };
        self.visiting.push(id.clone());
        let mut dependencies = BTreeSet::new();
        self.collect(
            &task,
            &task.project,
            &mut dependencies,
            &mut BTreeSet::new(),
        )?;
        self.visiting.pop();
        self.tasks.insert(
            id.clone(),
            Task {
                dependencies,
                ..task
            },
        );
        Ok(Some(id))
    }

    /// Resolves `dependsOn` with project dependencies taken from `derive_from`.
    /// Like Nx, a dependency without the target stands in for the task: the
    /// whole `dependsOn` list is applied again from there, so `^tsc` reaches
    /// the nearest projects that have `tsc`.
    fn collect(
        &mut self,
        task: &Task,
        derive_from: &str,
        dependencies: &mut BTreeSet<String>,
        seen: &mut BTreeSet<String>,
    ) -> Result<()> {
        if !seen.insert(derive_from.to_owned()) {
            return Ok(());
        }
        for dependency in task.definition.depends_on.as_deref().unwrap_or_default() {
            let (requests, derived) = self
                .dependencies(task, derive_from, dependency)
                .with_context(|| format!("dependsOn of {}", task.id))?;
            for request in requests {
                let has_target = self.workspace.projects[&request.project]
                    .targets
                    .contains_key(&request.target);
                if derived && !has_target {
                    let project = request.project;
                    self.collect(task, &project, dependencies, seen)?;
                } else if request.project == task.project && request.target == task.target {
                    // Reached back to itself through a dependency cycle.
                    continue;
                } else if let Some(id) = self.visit(request, false)? {
                    dependencies.insert(id);
                }
            }
        }
        Ok(())
    }

    /// The requests one `dependsOn` entry makes, and whether its projects are
    /// `derive_from`'s dependencies rather than named ones.
    fn dependencies(
        &self,
        task: &Task,
        derive_from: &str,
        value: &Value,
    ) -> Result<(Vec<Request>, bool)> {
        let (target, projects, forward, derived) = match value {
            Value::String(value) => {
                if let Some(target) = value.strip_prefix('^') {
                    (
                        target.to_owned(),
                        self.project_dependencies(derive_from),
                        false,
                        true,
                    )
                } else if let Some((project, target)) = value.split_once(':') {
                    (target.to_owned(), vec![project.to_owned()], false, false)
                } else {
                    (value.clone(), vec![task.project.clone()], false, false)
                }
            }
            Value::Object(object) => {
                let target = object
                    .get("target")
                    .and_then(Value::as_str)
                    .context("dependency target must be a string")?;
                let forward = match object.get("params").and_then(Value::as_str) {
                    None if !object.contains_key("params") => false,
                    Some("ignore") => false,
                    Some("forward") => true,
                    _ => bail!("dependency params must be forward or ignore"),
                };
                for key in object.keys() {
                    if !matches!(
                        key.as_str(),
                        "target" | "projects" | "dependencies" | "params"
                    ) {
                        bail!("unsupported dependency field {key:?}");
                    }
                }
                let dependencies = match object.get("dependencies") {
                    Some(value) => value
                        .as_bool()
                        .context("dependency dependencies must be boolean")?,
                    None => false,
                };
                if dependencies && object.contains_key("projects") {
                    bail!("dependency cannot specify both projects and dependencies: true");
                }
                let projects = if dependencies {
                    self.project_dependencies(derive_from)
                } else if let Some(selectors) = object.get("projects") {
                    let mut selectors: Vec<String> = match selectors {
                        Value::String(selector) => vec![selector.clone()],
                        value => serde_json::from_value(value.clone())
                            .context("dependency projects must be a string or string array")?,
                    };
                    for selector in &mut selectors {
                        if selector == "self" {
                            *selector = task.project.clone();
                        } else if selector == "!self" {
                            *selector = format!("!{}", task.project);
                        }
                    }
                    select_projects(&self.workspace.projects, &selectors, &[])?
                        .into_iter()
                        .collect()
                } else {
                    vec![task.project.clone()]
                };
                (target.to_owned(), projects, forward, dependencies)
            }
            _ => bail!("dependency must be a string or object"),
        };
        if target.is_empty() || target.contains(':') || target.starts_with('^') {
            bail!("invalid dependency target {target:?}");
        }
        let requests = projects
            .into_iter()
            .map(|project| Request {
                project,
                target: target.clone(),
                configuration: task.configuration.clone(),
                args: if forward {
                    task.args.clone()
                } else {
                    Vec::new()
                },
            })
            .collect();
        Ok((requests, derived))
    }

    fn project_dependencies(&self, project: &str) -> Vec<String> {
        // A pair can have several edge kinds; each dependency counts once.
        self.projects.dependencies[project]
            .iter()
            .map(|edge| edge.target.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }
}
