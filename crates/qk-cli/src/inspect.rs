//! Target inspection uses the same planning and path resolution as execution,
//! without preparing commands, loading dotenv files or running runtime inputs.
use std::collections::BTreeSet;
use std::io::{self, Write};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use qk_config::Workspace;
use qk_taskgraph::{Request, TaskGraph};
use serde_json::json;

#[derive(Args)]
pub struct Options {
    /// project:target[:configuration], or a target in the current project.
    target: Option<String>,
    #[arg(short = 'c', long, global = true)]
    configuration: Option<String>,
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// List resolved input files and declared environment/runtime inputs.
    Inputs(Selection),
    /// List resolved output paths and the outputs currently on disk.
    Outputs(Selection),
}

#[derive(Args)]
struct Selection {
    target: String,
    /// Check files, directories, or (for inputs) declared named values.
    #[arg(long, num_args = 1..)]
    check: Vec<String>,
}

/// Inspect a target with no executor preflight or command side effects.
pub fn show(workspace: &Workspace, options: Options) -> Result<i32> {
    let (name, check) = match &options.command {
        Some(Command::Inputs(selection) | Command::Outputs(selection)) => {
            if options.target.is_some() {
                bail!("name the target after inputs or outputs");
            }
            (&selection.target, selection.check.as_slice())
        }
        None => (
            options
                .target
                .as_ref()
                .context("show target needs a target")?,
            &[][..],
        ),
    };
    let name = if name.contains(':') {
        name.clone()
    } else {
        format!("{}:{name}", crate::current_project(workspace)?)
    };
    let mut request = Request::parse_in(workspace, &name)?;
    if options.configuration.is_some() {
        request.requested_configuration = options.configuration.clone();
        request.configuration = options.configuration;
    }
    let graph = TaskGraph::build(workspace, &[request])?;
    let task = &graph.tasks[graph.roots.first().context("target has no task")?];
    let mut data = json!({"project":task.project, "target":task.target});
    let mut members = BTreeSet::new();
    let mut paths = BTreeSet::new();
    match options.command {
        Some(Command::Inputs(_)) => {
            let mut resolved = qk_cache::resolve_tasks(workspace, &graph, &[])?;
            let inputs = resolved.remove(&task.id).context("target inputs missing")?;
            paths = inputs.files;
            data["files"] = json!(paths);
            for (prefix, category) in [
                ("env:", "environment"),
                ("runtime:", "runtime"),
                ("dependentTasksOutputFiles:", "depOutputs"),
            ] {
                let values: Vec<_> = inputs
                    .values
                    .keys()
                    .filter_map(|key| key.strip_prefix(prefix))
                    .collect();
                members.extend(values.iter().map(|value| (*value).to_owned()));
                if !values.is_empty() {
                    data[category] = json!(values);
                }
            }
            members.extend(inputs.external.iter().cloned());
            if !inputs.external.is_empty() {
                data["external"] = json!(inputs.external);
            }
        }
        Some(Command::Outputs(_)) => {
            let (outputs, _) = qk_cache::resolved_outputs(workspace, task)?;
            paths = qk_cache::Outputs::new(workspace, task)?.paths(&workspace.root)?;
            data["outputPaths"] = json!(outputs);
            data["expandedOutputs"] = json!(paths);
            let mut unresolved = Vec::new();
            for template in task.definition.outputs.iter().flatten() {
                let mut single = task.clone();
                single.definition.outputs = Some(vec![template.clone()]);
                if qk_cache::resolved_outputs(workspace, &single)?.0.is_empty() {
                    unresolved.push(template);
                }
            }
            data["unresolvedOutputs"] = json!(unresolved);
            if !check.is_empty() {
                let output_set = qk_cache::Outputs::new(workspace, task)?;
                members.extend(
                    check
                        .iter()
                        .filter(|path| output_set.matches(&normalized(path)))
                        .cloned(),
                );
            }
        }
        None => {
            let definition = &task.definition;
            data["executor"] = json!(definition.executor);
            data["options"] = json!(definition.options);
            data["parallelism"] = json!(definition.parallelism.unwrap_or(true));
            data["continuous"] = json!(definition.continuous.unwrap_or(false));
            data["cache"] = json!(definition.cache.unwrap_or(false));
            if let Some(configuration) = &task.configuration {
                data["configuration"] = json!(configuration);
            }
            if let Some(inputs) = &definition.inputs {
                data["inputs"] = json!(inputs);
            }
            if let Some(outputs) = &definition.outputs {
                data["outputs"] = json!(outputs);
            }
            if !task.dependencies.is_empty() {
                data["dependsOn"] = json!(task.dependencies);
            }
            let transitive: BTreeSet<_> = graph
                .tasks
                .keys()
                .filter(|id| **id != task.id && !task.dependencies.contains(*id))
                .collect();
            if !transitive.is_empty() {
                data["transitiveTasks"] = json!(transitive);
            }
            if !definition.configurations.is_empty() {
                data["configurations"] =
                    json!(definition.configurations.keys().collect::<Vec<_>>());
            }
            if let Some(default) = &definition.default_configuration {
                data["defaultConfiguration"] = json!(default);
            }
            if let Some(command) = definition
                .options
                .get("command")
                .or_else(|| definition.options.get("script"))
            {
                data["command"] = command.clone();
            }
        }
    }
    if !check.is_empty() {
        let mut results = Vec::new();
        let mut failed = false;
        for value in check {
            let path = normalized(value);
            let contained: Vec<_> = paths
                .iter()
                .filter(|file| path.is_empty() || file.starts_with(&format!("{path}/")))
                .collect();
            let matched = members.contains(value) || paths.contains(&path) || !contained.is_empty();
            failed |= !matched;
            results.push(json!({"value":value, "matched":matched, "files":contained}));
            if !options.json {
                writeln!(
                    io::stdout().lock(),
                    "{}: {}",
                    value,
                    if matched { "matched" } else { "not matched" }
                )?;
            }
        }
        if options.json {
            crate::print_json(
                &json!({"project":task.project,"target":task.target,"checks":results}),
            )?;
        }
        return Ok(i32::from(failed));
    }
    if options.json || options.command.is_none() {
        crate::print_json(&data)?;
    } else {
        for path in paths {
            writeln!(io::stdout().lock(), "{path}")?;
        }
        for value in members {
            writeln!(io::stdout().lock(), "{value}")?;
        }
    }
    Ok(0)
}

/// Normalize workspace-relative query paths, including directory queries.
fn normalized(path: &str) -> String {
    let path = path
        .replace('\\', "/")
        .trim_start_matches("./")
        .trim_end_matches('/')
        .to_owned();
    if path == "." { String::new() } else { path }
}
