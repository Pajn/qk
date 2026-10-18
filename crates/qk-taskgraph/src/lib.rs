//! Expand requested targets into a validated task DAG before executing anything.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};
use qk_config::{Target, Workspace};
use qk_graph::ProjectGraph;
use serde::Serialize;
use serde_json::{Map, Value};

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
    /// The `inputs` and `waitFor` of the `dependsOn` entries that named each
    /// dependency, where they differ from the defaults. They only matter for
    /// a continuous dependency.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub edges: BTreeMap<String, Edge>,
}

impl Task {
    /// How this task depends on `dependency`.
    pub fn edge(&self, dependency: &str) -> Edge {
        self.edges.get(dependency).copied().unwrap_or_default()
    }
}

/// How a task depends on a continuous task, from its `dependsOn` entries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Edge {
    /// Whether the continuous task's inputs count for this task. Nx adds them
    /// to the dependent's hash and affected selection unless the entry sets
    /// `inputs: false`.
    pub inputs: bool,
    pub wait_for: WaitFor,
}

impl Default for Edge {
    fn default() -> Self {
        Self {
            inputs: true,
            wait_for: WaitFor::Started,
        }
    }
}

impl Edge {
    /// Two entries naming one dependency, as Nx merges them: waiting for
    /// readiness wins, and the inputs count unless every entry leaves them out.
    fn merge(self, other: Self) -> Self {
        Self {
            inputs: self.inputs || other.inputs,
            wait_for: self.wait_for.max(other.wait_for),
        }
    }
}

/// When a task depending on a continuous task starts: once that task has
/// started, or once its target-level `readyWhen` check has passed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum WaitFor {
    #[default]
    Started,
    Ready,
}

/// The dependencies `dependsOn` resolves to, with how each is depended on.
#[derive(Default)]
struct Dependencies {
    ids: BTreeSet<String>,
    edges: BTreeMap<String, Edge>,
}

impl Dependencies {
    fn insert(&mut self, id: String, edge: Edge) {
        let edge = if self.ids.insert(id.clone()) {
            edge
        } else {
            self.edges.get(&id).copied().unwrap_or_default().merge(edge)
        };
        if edge == Edge::default() {
            self.edges.remove(&id);
        } else {
            self.edges.insert(id, edge);
        }
    }
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
            let cli = request.args.clone();
            let visit = Visit {
                request: request.clone(),
                overrides: Map::new(),
                cli: &cli,
            };
            roots.insert(builder.visit(visit, true)?.with_context(|| {
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
                task.edges.retain(|id, _| roots.contains(id));
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
                    let task = graph.get_mut(id).unwrap();
                    task.dependencies.remove(&dependency);
                    task.edges.remove(&dependency);
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

/// What one `dependsOn` entry declares: its requests, the options it
/// forwards, whether its projects are dependencies rather than named ones,
/// and how it depends on them.
struct Declared {
    requests: Vec<Request>,
    overrides: Map<String, Value>,
    derived: bool,
    edge: Edge,
}

/// A task to visit: its request, the options a dependent forwards to it,
/// and the run's own arguments, which `params: "forward"` passes on.
struct Visit<'c> {
    request: Request,
    overrides: Map<String, Value>,
    cli: &'c [String],
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
    fn visit(&mut self, visit: Visit, required: bool) -> Result<Option<String>> {
        let Visit {
            request,
            overrides,
            cli,
        } = visit;
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
        if self
            .visited
            .contains(&(id.clone(), requested_configuration.clone()))
        {
            return Ok(Some(id));
        }
        // As in Nx, a task reached again keeps the arguments and forwarded
        // options it was first reached with.
        let (args, definition) = match self.tasks.get(&id) {
            Some(existing) => (existing.args.clone(), existing.definition.clone()),
            None => {
                let mut definition = target.clone();
                if let Some(name) = &configuration {
                    let options = target
                        .configurations
                        .get(name)
                        .with_context(|| format!("{id}: default configuration does not exist"))?;
                    for (key, value) in options {
                        if key == "env" {
                            value.as_object().with_context(|| {
                                format!("{id}: configuration env must be an object")
                            })?;
                        }
                        definition.options.insert(key.clone(), value.clone());
                    }
                }
                // Forwarded options win over the target's own, as Nx's
                // overrides do.
                definition.options.extend(overrides);
                (request.args, definition)
            }
        };
        // As in Nx, creating a task validates its outputs.
        if let Some(outputs) = &definition.outputs {
            validate_outputs(outputs)?;
        }
        let task = Task {
            id: id.clone(),
            project: request.project,
            target: request.target,
            configuration,
            args,
            definition,
            dependencies: BTreeSet::new(),
            edges: BTreeMap::new(),
        };
        self.visiting.push(id.clone());
        let mut dependencies = self
            .tasks
            .get(&id)
            .map(|task| Dependencies {
                ids: task.dependencies.clone(),
                edges: task.edges.clone(),
            })
            .unwrap_or_default();
        self.collect(
            &task,
            &task.project,
            requested_configuration.as_deref(),
            cli,
            &mut dependencies,
            &mut BTreeSet::new(),
        )?;
        self.visiting.pop();
        self.visited.insert((id.clone(), requested_configuration));
        self.tasks.insert(
            id.clone(),
            Task {
                dependencies: dependencies.ids,
                edges: dependencies.edges,
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
        cli: &[String],
        dependencies: &mut Dependencies,
        seen: &mut BTreeSet<String>,
    ) -> Result<()> {
        if !seen.insert(derive_from.to_owned()) {
            return Ok(());
        }
        for dependency in task.definition.depends_on.as_deref().unwrap_or_default() {
            let Declared {
                requests,
                overrides,
                derived,
                edge,
            } = self
                .dependencies(task, derive_from, requested_configuration, cli, dependency)
                .with_context(|| format!("dependsOn of {}", task.id))?;
            for request in requests {
                let has_target = self.workspace.projects[&request.project]
                    .targets
                    .contains_key(&request.target);
                if derived && !has_target {
                    let project = request.project;
                    self.collect(
                        task,
                        &project,
                        requested_configuration,
                        cli,
                        dependencies,
                        seen,
                    )?;
                } else if request.project == task.project && request.target == task.target {
                    // Reached back to itself through a dependency cycle.
                    continue;
                } else {
                    let visit = Visit {
                        request,
                        overrides: overrides.clone(),
                        cli,
                    };
                    if let Some(id) = self.visit(visit, false)? {
                        dependencies.insert(id, edge);
                    }
                }
            }
        }
        Ok(())
    }

    /// What one `dependsOn` entry declares, with project dependencies taken
    /// from `derive_from`.
    fn dependencies(
        &self,
        task: &Task,
        derive_from: &str,
        requested_configuration: Option<&str>,
        cli: &[String],
        value: &Value,
    ) -> Result<Declared> {
        let mut forward_options = false;
        let mut edge = Edge::default();
        let (target, projects, forward, derived) = match value {
            Value::String(value) => {
                if let Some(target) = value.strip_prefix('^') {
                    (
                        target.to_owned(),
                        self.project_dependencies(derive_from),
                        false,
                        true,
                    )
                } else {
                    let (project, target) = self.target_reference(value, &task.project);
                    (target, vec![project], false, false)
                }
            }
            Value::Object(object) => {
                // As in Nx, fields it does not know are ignored, and any
                // `params` or `options` but `forward` leaves them behind.
                let target = object
                    .get("target")
                    .and_then(Value::as_str)
                    .context("dependency target must be a string")?;
                let forward = object.get("params").and_then(Value::as_str) == Some("forward");
                forward_options = object.get("options").and_then(Value::as_str) == Some("forward");
                if let Some(inputs) = object.get("inputs") {
                    edge.inputs = inputs
                        .as_bool()
                        .context("dependency inputs must be boolean")?;
                }
                edge.wait_for = match object.get("waitFor").and_then(Value::as_str) {
                    None if !object.contains_key("waitFor") => WaitFor::Started,
                    Some("started") => WaitFor::Started,
                    Some("ready") => WaitFor::Ready,
                    _ => bail!("dependency waitFor must be started or ready"),
                };
                let dependencies = match object.get("dependencies") {
                    Some(value) => value
                        .as_bool()
                        .context("dependency dependencies must be boolean")?,
                    None => false,
                };
                // Named projects win over `dependencies: true`, as in Nx.
                let projects = if let Some(selectors) = object.get("projects") {
                    let selectors: Vec<String> = match selectors {
                        // Nx 24 dropped the Nx 15 spellings: a task's own
                        // project is no `projects`, its dependencies `^target`.
                        Value::String(selector)
                            if matches!(selector.as_str(), "self" | "dependencies")
                                && !self.workspace.projects.contains_key(selector) =>
                        {
                            bail!(
                                "projects: {selector:?} is no longer supported; leave projects \
                                 out for the task's own project, or use dependencies: true"
                            )
                        }
                        Value::String(selector) => vec![selector.clone()],
                        value => serde_json::from_value(value.clone())
                            .context("dependency projects must be a string or string array")?,
                    };
                    qk_config::find_matching_projects(
                        &selectors,
                        &self.workspace.projects,
                        qk_config::Reader::Nx,
                    )
                } else if dependencies {
                    self.project_dependencies(derive_from)
                } else {
                    vec![task.project.clone()]
                };
                let derived = dependencies && !object.contains_key("projects");
                (target.to_owned(), projects, forward, derived)
            }
            _ => bail!("dependency must be a string or object"),
        };
        // As in Nx, a glob stands for every target name in the workspace it
        // matches as minimatch matches it, each then depended on as if named.
        let targets: Vec<String> = if qk_config::is_glob(&target) {
            let matcher = qk_config::minimatch::Minimatch::new(&target, false);
            self.workspace
                .projects
                .values()
                .flat_map(|project| project.targets.keys())
                .filter(|name| matcher.is_match(name))
                .cloned()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect()
        } else {
            vec![target]
        };
        // As in Nx, `options: "forward"` passes the target's options, its
        // configuration's included, as the dependency's own, and
        // `params: "forward"` the run's arguments.
        let mut overrides = Map::new();
        if forward_options {
            let source = &self.workspace.projects[&task.project].targets[&task.target];
            overrides.extend(source.options.clone());
            if let Some(options) = task
                .configuration
                .as_ref()
                .and_then(|name| source.configurations.get(name))
            {
                overrides.extend(options.clone());
            }
        }
        let args = if forward { cli.to_vec() } else { Vec::new() };
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
        Ok(Declared {
            requests,
            overrides,
            derived,
            edge,
        })
    }

    /// Nx's `readProjectAndTargetFromTargetString`: `project:target` when
    /// the first part names a project, and otherwise a target of the task's
    /// own project, colons and all.
    fn target_reference(&self, text: &str, current: &str) -> (String, String) {
        let parts = split_target(text, &self.workspace.projects, current);
        match &parts[..] {
            [target] => (current.to_owned(), target.clone()),
            [project, rest @ ..] if self.workspace.projects.contains_key(project) => {
                (project.clone(), rest.join(":"))
            }
            _ => (current.to_owned(), text.to_owned()),
        }
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

/// Nx's `splitByColons`: parts between colons, a double-quoted run keeping
/// its colons.
fn split_by_colons(text: &str) -> Vec<String> {
    let mut parts = vec![String::new()];
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            ':' => parts.push(String::new()),
            '"' => {
                for c in chars.by_ref() {
                    if c == '"' {
                        break;
                    }
                    parts.last_mut().unwrap().push(c);
                }
            }
            c => parts.last_mut().unwrap().push(c),
        }
    }
    parts
}

/// Every `[project, target?, configuration?]` reading of the parts that
/// names a project and, when given, one of its targets and configurations;
/// read on the current project too, as a bare target.
fn readings(
    parts: &[String],
    projects: &BTreeMap<String, qk_config::Project>,
    current: &str,
) -> Vec<Vec<String>> {
    let mut readings = Vec::new();
    let read = |project: &str, rest: &[String], readings: &mut Vec<Vec<String>>| {
        let targets = &projects[project].targets;
        for end in 1..=rest.len() {
            let target = rest[..end].join(":");
            let Some(definition) = targets.get(&target) else {
                continue;
            };
            if end == rest.len() {
                readings.push(vec![project.to_owned(), target]);
            } else {
                let configuration = rest[end..].join(":");
                if definition.configurations.contains_key(&configuration) {
                    readings.push(vec![project.to_owned(), target, configuration]);
                }
            }
        }
    };
    if projects.contains_key(current) {
        read(current, parts, &mut readings);
    }
    for end in 1..=parts.len() {
        let project = parts[..end].join(":");
        if !projects.contains_key(&project) {
            continue;
        }
        if end == parts.len() {
            readings.push(vec![project]);
        } else {
            read(&project, &parts[end..], &mut readings);
        }
    }
    readings
}

/// The reading Nx prefers: on the current project, then the longest
/// project, target and configuration names.
fn best(readings: Vec<Vec<String>>, current: &str) -> Option<Vec<String>> {
    let rank = |reading: &Vec<String>| {
        let length = |index: usize| reading.get(index).map_or(0, String::len);
        (reading[0] == current, length(0), length(1), length(2))
    };
    let mut best: Option<Vec<String>> = None;
    for reading in readings {
        if best.as_ref().is_none_or(|best| rank(&reading) > rank(best)) {
            best = Some(reading);
        }
    }
    best
}

/// Nx's `splitTargetFromNodes`.
fn split_target(
    text: &str,
    projects: &BTreeMap<String, qk_config::Project>,
    current: &str,
) -> Vec<String> {
    let parts = split_by_colons(text);
    if let Some(best) = best(readings(&parts, projects, current), current) {
        return best;
    }
    let colon = match text.find(':') {
        Some(0) => text[1..].find(':').map(|index| index + 1),
        colon => colon,
    };
    if colon.is_none_or(|colon| colon == 0) {
        return vec![text.to_owned()];
    }
    let mut project = parts[0].clone();
    let mut rest = parts[1..].to_vec();
    // A leading colon starts the longest known project name it can.
    if project.is_empty() && !rest.is_empty() {
        let mut absorbed = 1;
        for count in (1..rest.len()).rev() {
            if projects.contains_key(&format!(":{}", rest[..count].join(":"))) {
                absorbed = count;
                break;
            }
        }
        project = format!(":{}", rest[..absorbed].join(":"));
        rest.drain(..absorbed);
    }
    let Some(configuration) = rest.last().cloned() else {
        return vec![project];
    };
    // The project and target, with a configuration they do not have.
    if let Some(head) = text.get(..text.len().saturating_sub(configuration.len() + 1))
        && let Some(best) = best(readings(&split_by_colons(head), projects, current), current)
        && best.len() == 2
    {
        return vec![best[0].clone(), best[1].clone(), configuration];
    }
    let mut result = vec![project.clone()];
    let targets = projects.get(&project).map(|project| &project.targets);
    for end in (1..=rest.len()).rev() {
        let target = rest[..end].join(":");
        if targets.is_some_and(|targets| targets.contains_key(&target)) {
            result.push(target);
            if end < rest.len() {
                result.push(rest[end..].join(":"));
            }
            return result;
        }
    }
    result.extend(rest);
    result
}

/// Nx 24's `validateOutputs`: each output starts with a token, `!` before it
/// if negated, and one from `{workspaceRoot}` names a directory before any
/// pattern. The error lists every output that breaks either rule, in Nx's
/// words.
pub fn validate_outputs(outputs: &[String]) -> Result<()> {
    let missing_prefix: Vec<&String> = outputs
        .iter()
        .filter(|output| {
            // Nx's `^!?\{[\s\S]+\}`: a `{` and, after at least one more
            // character, a `}`.
            let mut chars = output.strip_prefix('!').unwrap_or(output).chars();
            !(chars.next() == Some('{') && chars.next().is_some() && chars.any(|c| c == '}'))
        })
        .collect();
    let workspace_globs: Vec<&String> = outputs
        .iter()
        .filter(|output| !missing_prefix.contains(output))
        .filter(|output| {
            ["!{workspaceRoot}", "{workspaceRoot}"]
                .iter()
                .any(|prefix| output.strip_prefix(prefix).is_some_and(has_no_directory))
        })
        .collect();
    let mut message = String::new();
    if !missing_prefix.is_empty() {
        message.push_str(&format!(
            "The following outputs are invalid: \n{}\n\nRun `nx repair` to fix this.",
            missing_prefix
                .iter()
                .map(|output| format!(
                    " - {output}\n   ** Reason: Outputs must start with either \"{{workspaceRoot}}/\" or \"{{projectRoot}}/\"."
                ))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    if !workspace_globs.is_empty() {
        message.push_str(&format!(
            "The following outputs are defined by a glob pattern from the workspace root: \n - {}\n\nThese can be slow, replace them with a more specific pattern.",
            workspace_globs
                .iter()
                .map(|output| output.as_str())
                .collect::<Vec<_>>()
                .join("\n - ")
        ));
    }
    if message.is_empty() {
        Ok(())
    } else {
        bail!(message)
    }
}

/// Whether `glob` has a pattern before any named directory, as Nx's
/// `partition_glob` reads it: its leading segments with no glob syntax once
/// their escapes are read join to nothing. `@` and `+` start no pattern
/// without a group after them.
fn has_no_directory(glob: &str) -> bool {
    let mut directory = Vec::new();
    for segment in glob.split('/') {
        let mut syntax = false;
        let mut chars = segment.chars();
        while let Some(c) = chars.next() {
            match c {
                '\\' => {
                    chars.next();
                }
                '*' | '?' | '[' | ']' | '{' | '}' | '(' | ')' | '!' => syntax = true,
                _ => {}
            }
        }
        if syntax {
            return directory.join("/").is_empty();
        }
        directory.push(segment);
    }
    false
}
