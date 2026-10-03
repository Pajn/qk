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
    /// Configuration name propagated to dependencies, before root fallback.
    pub requested_configuration: Option<String>,
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
            requested_configuration: parts.get(2).map(|part| (*part).into()),
            args: Vec::new(),
        })
    }

    /// Resolve a literal target name before treating its final segment as a configuration.
    pub fn parse_in(workspace: &Workspace, value: &str) -> Result<Self> {
        let Some((project, identifier)) = value.split_once(':') else {
            return Self::parse(value);
        };
        if let Some(project_config) = workspace.projects.get(project) {
            if project_config.targets.contains_key(identifier) {
                return Ok(Self {
                    project: project.into(),
                    target: identifier.into(),
                    configuration: None,
                    requested_configuration: None,
                    args: Vec::new(),
                });
            }
            if let Some((target, configuration)) = identifier.rsplit_once(':')
                && project_config.targets.contains_key(target)
                && !configuration.is_empty()
            {
                return Ok(Self {
                    project: project.into(),
                    target: target.into(),
                    configuration: Some(configuration.into()),
                    requested_configuration: Some(configuration.into()),
                    args: Vec::new(),
                });
            }
        }
        Self::parse(value)
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
    /// The cycles broken under `ignore_cycles`, each as the tasks around it.
    #[serde(skip)]
    pub cycles: Vec<Vec<String>>,
}

/// Nx's options for building a task graph.
#[derive(Clone, Copy, Debug, Default)]
pub struct BuildOptions {
    /// `--exclude-task-dependencies`: only the requested tasks, without the
    /// tasks they depend on.
    pub exclude_task_dependencies: bool,
    /// `--nx-ignore-cycles`: break each cycle rather than failing, as Nx's
    /// `makeAcyclic` does, by dropping the dependency that closes it.
    pub ignore_cycles: bool,
}

impl TaskGraph {
    pub fn build(workspace: &Workspace, requests: &[Request]) -> Result<Self> {
        Self::build_with(workspace, requests, BuildOptions::default())
    }

    pub fn build_with(
        workspace: &Workspace,
        requests: &[Request],
        options: BuildOptions,
    ) -> Result<Self> {
        if requests.is_empty() {
            bail!("no tasks selected");
        }
        let projects = ProjectGraph::build(workspace)?;
        let mut builder = Builder {
            workspace,
            projects,
            tasks: BTreeMap::new(),
            visiting: Vec::new(),
            visited: BTreeSet::new(),
            ignore_cycles: options.ignore_cycles,
            cycles: Vec::new(),
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
        let mut graph = Self {
            roots,
            tasks: builder.tasks,
            cycles: builder.cycles,
        };
        if !graph.cycles.is_empty() {
            graph.make_acyclic();
        }
        if options.exclude_task_dependencies {
            let roots = graph.roots.clone();
            graph.tasks.retain(|id, _| roots.contains(id));
            for task in graph.tasks.values_mut() {
                task.dependencies.retain(|id| roots.contains(id));
            }
        }
        Ok(graph)
    }

    /// Nx's `makeAcyclic`: from each task, depth first, a dependency on a
    /// task already on the path is dropped.
    fn make_acyclic(&mut self) {
        fn visit(
            graph: &mut BTreeMap<String, Task>,
            id: &str,
            visited: &mut BTreeSet<String>,
            path: &mut Vec<String>,
        ) {
            if !visited.insert(id.to_owned()) {
                return;
            }
            let dependencies: Vec<String> = graph[id].dependencies.iter().cloned().collect();
            for dependency in dependencies {
                if path.contains(&dependency) {
                    graph.get_mut(id).unwrap().dependencies.remove(&dependency);
                } else {
                    path.push(dependency.clone());
                    visit(graph, &dependency, visited, path);
                    path.pop();
                }
            }
        }
        let ids: Vec<String> = self.tasks.keys().cloned().collect();
        let mut visited = BTreeSet::new();
        for id in ids {
            let mut path = vec![id.clone()];
            visit(&mut self.tasks, &id, &mut visited, &mut path);
        }
    }
}

struct Builder<'a> {
    workspace: &'a Workspace,
    projects: ProjectGraph,
    tasks: BTreeMap<String, Task>,
    visiting: Vec<String>,
    visited: BTreeSet<(String, Option<String>)>,
    ignore_cycles: bool,
    cycles: Vec<Vec<String>>,
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
        let requested_configuration = request.requested_configuration.clone();
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
            cycle.push(id.clone());
            if !self.ignore_cycles {
                bail!("task dependency cycle: {}", cycle.join(" -> "));
            }
            // The edge stays until the graph is complete, then the cycle is broken.
            self.cycles.push(cycle);
            return Ok(Some(id));
        }
        if let Some(existing) = self.tasks.get(&id) {
            if existing.args != request.args {
                bail!("task {id} requested with conflicting forwarded arguments");
            }
            if self
                .visited
                .contains(&(id.clone(), requested_configuration.clone()))
            {
                return Ok(Some(id));
            }
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
                } else {
                    if key == "env" {
                        value.as_object().with_context(|| {
                            format!("{id}: configuration env must be an object")
                        })?;
                    }
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
        let mut dependencies = self
            .tasks
            .get(&id)
            .map(|task| task.dependencies.clone())
            .unwrap_or_default();
        self.collect(
            &task,
            &task.project,
            requested_configuration.as_deref(),
            &mut dependencies,
            &mut BTreeSet::new(),
        )?;
        self.visiting.pop();
        self.visited.insert((id.clone(), requested_configuration));
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
        requested_configuration: Option<&str>,
        dependencies: &mut BTreeSet<String>,
        seen: &mut BTreeSet<String>,
    ) -> Result<()> {
        if !seen.insert(derive_from.to_owned()) {
            return Ok(());
        }
        for dependency in task.definition.depends_on.as_deref().unwrap_or_default() {
            let (requests, derived) = self
                .dependencies(task, derive_from, requested_configuration, dependency)
                .with_context(|| format!("dependsOn of {}", task.id))?;
            for request in requests {
                let has_target = self.workspace.projects[&request.project]
                    .targets
                    .contains_key(&request.target);
                if derived && !has_target {
                    let project = request.project;
                    self.collect(task, &project, requested_configuration, dependencies, seen)?;
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
        requested_configuration: Option<&str>,
        value: &Value,
    ) -> Result<(Vec<Request>, bool)> {
        let mut forward_options = false;
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
                forward_options = match object.get("options").and_then(Value::as_str) {
                    None if !object.contains_key("options") => false,
                    Some("ignore") => false,
                    Some("forward") => true,
                    _ => bail!("dependency options must be forward or ignore"),
                };
                for key in object.keys() {
                    if !matches!(
                        key.as_str(),
                        "target" | "projects" | "dependencies" | "params" | "options"
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
        // As in Nx, a glob stands for every target name in the workspace it
        // matches, each then depended on as if named.
        let targets: Vec<String> = if qk_config::is_glob(&target) {
            let matcher = globset::Glob::new(&target)
                .with_context(|| format!("invalid dependency target {target:?}"))?
                .compile_matcher();
            self.workspace
                .projects
                .values()
                .flat_map(|project| project.targets.keys())
                .filter(|name| matcher.is_match(name.as_str()))
                .cloned()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect()
        } else {
            vec![target]
        };
        // As in Nx, `options: "forward"` passes the task's options, its
        // configuration's included, as overrides, before any forwarded
        // arguments so that those win.
        let mut args = Vec::new();
        if forward_options {
            for (key, value) in &task.definition.options {
                option_flags(key, value, &mut args);
            }
        }
        if forward {
            args.extend(task.args.iter().cloned());
        }
        let requests = targets
            .iter()
            .flat_map(|target| {
                projects.iter().map(|project| Request {
                    project: project.clone(),
                    target: target.clone(),
                    configuration: requested_configuration.map(str::to_owned),
                    requested_configuration: requested_configuration.map(str::to_owned),
                    args: args.clone(),
                })
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

/// An option as the flags that set it: `--name=value`, and `--name.key=value`
/// for each field of an object. A list cannot be spelled as a flag, and is
/// left out.
fn option_flags(key: &str, value: &Value, flags: &mut Vec<String>) {
    match value {
        Value::Object(fields) => {
            for (field, value) in fields {
                option_flags(&format!("{key}.{field}"), value, flags);
            }
        }
        Value::String(text) => flags.push(format!("--{key}={text}")),
        Value::Number(number) => flags.push(format!("--{key}={number}")),
        Value::Bool(flag) => flags.push(format!("--{key}={flag}")),
        Value::Array(_) | Value::Null => {}
    }
}
