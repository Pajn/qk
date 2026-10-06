//! `qk show hash`: what a task's cache key is computed from, and where it
//! differs from another task's or from the task's in a recorded run.

use std::collections::BTreeMap;
use std::io::Write;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, bail};
use qk_config::Workspace;
use qk_taskgraph::{Request, TaskGraph};
use serde_json::{Value, json};

/// What a key is compared with.
pub enum Against {
    Task(Request),
    Run(String),
}

/// A key, where it comes from, and its inputs unless a recorded run no longer
/// keeps them.
struct Keyed {
    task: String,
    run: Option<String>,
    key: String,
    inputs: Option<Value>,
}

impl Keyed {
    fn label(&self) -> String {
        match &self.run {
            Some(run) => format!("{} in run {run}", self.task),
            None => self.task.clone(),
        }
    }

    fn json(&self) -> Value {
        let mut value = json!({"task": self.task, "key": self.key});
        if let Some(run) = &self.run {
            value["run"] = json!(run);
        }
        if let Some(inputs) = &self.inputs {
            value["inputs"] = redacted(inputs);
        }
        value
    }
}

pub fn show(
    workspace: &Workspace,
    request: Request,
    against: Option<Against>,
    json: bool,
    out: &mut impl Write,
) -> Result<()> {
    let this = key_now(workspace, request)?;
    let other = match against {
        None => None,
        Some(Against::Task(request)) => Some(key_now(workspace, request)?),
        Some(Against::Run(run)) => {
            let Some((key, inputs)) = crate::history::open(workspace)?.key(&run, &this.task)?
            else {
                bail!("run {run} did not key {}", this.task);
            };
            Some(Keyed {
                task: this.task.clone(),
                run: Some(run),
                key,
                inputs,
            })
        }
    };
    let cause = other.as_ref().and_then(|other| {
        Some(qk_history::diff(
            other.inputs.as_ref()?,
            this.inputs.as_ref()?,
        ))
    });
    if json {
        let mut report = this.json();
        if let Some(other) = &other {
            report["against"] = other.json();
            report["same"] = json!(other.key == this.key);
            if let Some(cause) = cause.as_ref().filter(|_| other.key != this.key) {
                report["differences"] = serde_json::to_value(cause)?;
            }
        }
        serde_json::to_writer_pretty(&mut *out, &report)?;
        writeln!(out)?;
        return Ok(());
    }
    let Some(other) = other else {
        writeln!(out, "{}  {}", this.task, this.key)?;
        for line in summary(this.inputs.as_ref().expect("a computed key has inputs")) {
            writeln!(out, "  {line}")?;
        }
        return Ok(());
    };
    let (this_label, other_label) = (this.label(), other.label());
    let width = this_label.len().max(other_label.len());
    writeln!(out, "{this_label:width$}  {}", this.key)?;
    writeln!(out, "{other_label:width$}  {}", other.key)?;
    if other.key == this.key {
        writeln!(out, "same key")?;
        return Ok(());
    }
    let Some(cause) = cause else {
        writeln!(out, "the inputs of {other_label} are no longer kept")?;
        return Ok(());
    };
    writeln!(out, "keys differ in: {}", cause.changed.join(", "))?;
    for line in crate::history::differences(&cause) {
        writeln!(out, "{line}")?;
    }
    Ok(())
}

/// The key `qk run` would compute for the task now, in a graph of the task
/// and its dependencies as `qk run` builds it.
fn key_now(workspace: &Workspace, request: Request) -> Result<Keyed> {
    let graph = TaskGraph::build(workspace, &[request])?;
    let [task] = graph.roots.iter().collect::<Vec<_>>()[..] else {
        bail!("expected one task, got {:?}", graph.roots);
    };
    let environment: BTreeMap<_, _> = std::env::vars_os().collect();
    let prepared = graph
        .tasks
        .iter()
        .map(|(id, task)| {
            qk_executor::prepare(workspace, task, &environment)
                .with_context(|| format!("cannot execute {id}"))
                .map(|prepared| (id.clone(), prepared))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    let (key, inputs) = qk_cache::keys(workspace, &graph, &prepared, &AtomicBool::new(false))?
        .remove(task)
        .expect("every task is keyed")
        .map_err(anyhow::Error::msg)
        .with_context(|| format!("{task} has no key"))?;
    Ok(Keyed {
        task: task.clone(),
        run: None,
        key,
        inputs: Some(inputs),
    })
}

/// A line per part of what the key covers; `--json` has all of it.
fn summary(inputs: &Value) -> Vec<String> {
    let names = |key: &str| {
        inputs
            .get(key)
            .and_then(Value::as_object)
            .map(|object| object.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default()
    };
    let mut lines = Vec::new();
    let definition = &inputs["definition"];
    if let Some(executor) = definition.get("executor").and_then(Value::as_str) {
        lines.push(format!("executor    {executor}"));
    }
    if let Some(declared) = definition.get("inputs") {
        lines.push(format!("inputs      {declared}"));
    }
    if let Some(args) = inputs["args"].as_array().filter(|args| !args.is_empty()) {
        lines.push(format!("args        {}", Value::from(args.clone())));
    }
    lines.push(format!("files       {}", names("files").len()));
    for name in names("values") {
        lines.push(format!("value       {name}"));
    }
    for id in names("dependencies") {
        lines.push(format!("dependency  {id}"));
    }
    let platform = inputs["platform"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join(" ");
    lines.push(format!(
        "tooling     qk {} on {platform}",
        inputs["qk"].as_str().unwrap_or("?")
    ));
    lines
}

/// Inputs with env values replaced by their digest: they can hold
/// credentials, and a digest still tells two values apart.
fn redacted(inputs: &Value) -> Value {
    let mut inputs = inputs.clone();
    if let Some(values) = inputs.get_mut("values").and_then(Value::as_object_mut) {
        for (name, value) in values.iter_mut() {
            if name.starts_with("env:")
                && let Some(text) = value.as_str()
            {
                *value = json!({"blake3": blake3::hash(text.as_bytes()).to_hex().to_string()});
            }
        }
    }
    inputs
}
