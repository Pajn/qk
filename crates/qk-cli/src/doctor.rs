//! Read-only diagnostics for qk's explicit Nx compatibility boundary.
use anyhow::Result;
use qk_config::Workspace;
use serde::Serialize;
use std::io::{self, Write};

/// Nx CLI flags accepted for script compatibility without enabling their features.
const NOOP_OPTIONS: &[&str] = &[
    "--runner",
    "--batch",
    "--skip-sync",
    "--cloud",
    "--no-cloud",
    "--dte",
    "--no-dte",
    "--agents",
    "--tui",
    "--no-tui",
    "--tui-auto-exit",
];

#[derive(Serialize)]
struct Finding {
    severity: &'static str,
    code: &'static str,
    location: String,
    message: String,
}

/// Inspect normalized definitions without dotenv, executor preparation or plugins.
pub fn show(workspace: &Workspace, json: bool, strict: bool) -> Result<i32> {
    let mut findings = Vec::new();
    if let Some(plugins) = workspace.config.extra.get("plugins")
        && plugins
            .as_array()
            .is_some_and(|plugins| !plugins.is_empty())
    {
        findings.push(Finding {
            severity: "warning", code: "plugins-not-run", location: "nx.json.plugins".into(),
            message: "qk does not run Nx plugins or infer their targets; declare required targets explicitly.".into(),
        });
    }
    if workspace
        .config
        .extra
        .get("sync")
        .and_then(|sync| sync.get("globalGenerators"))
        .and_then(serde_json::Value::as_array)
        .is_some_and(|generators| !generators.is_empty())
    {
        findings.push(Finding {
            severity: "warning", code: "sync-generators-not-run",
            location: "nx.json.sync.globalGenerators".into(),
            message: "qk does not run workspace sync generators; perform required synchronization separately.".into(),
        });
    }
    for (project, definition) in &workspace.projects {
        for (target, definition) in &definition.targets {
            let location = format!("{project}:{target}");
            // Every target has an executor: one that runs nothing is a no-op
            // or left out as the workspace loads.
            if let Some(executor) = definition.executor.as_deref().filter(|executor| {
                !matches!(*executor, "nx:run-commands" | "nx:run-script" | "nx:noop")
            }) {
                findings.push(Finding {
                    severity: "error", code: "unsupported-executor", location: location.clone(),
                    message: format!("Executor {executor:?} is unsupported; use shell commands, package scripts or nx:noop."),
                });
            }
            for (key, fields) in [
                ("qk:warm", qk_cache::warm::FIELDS),
                ("qk:threads", qk_runner::threads::FIELDS),
            ] {
                let objects = match definition.extra.get(key) {
                    Some(serde_json::Value::Object(object)) => vec![object],
                    Some(serde_json::Value::Array(entries)) => entries
                        .iter()
                        .filter_map(serde_json::Value::as_object)
                        .collect(),
                    _ => Vec::new(),
                };
                for field in objects.into_iter().flat_map(|object| object.keys()) {
                    if !fields.contains(&field.as_str()) {
                        findings.push(Finding {
                            severity: "warning", code: "unknown-field", location: location.clone(),
                            message: format!("{key}.{field} is not a field this qk knows, so it is ignored; check its spelling or update qk."),
                        });
                    }
                }
            }
            if let Some(value) = definition.extra.get("qk:sandbox")
                && !value.is_boolean()
            {
                findings.push(Finding {
                    severity: "warning",
                    code: "invalid-field",
                    location: location.clone(),
                    message: format!("qk:sandbox is {value}, but only true or false is read, so it is ignored and the task is sandboxed."),
                });
            }
            if definition
                .extra
                .get("syncGenerators")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|items| !items.is_empty())
            {
                findings.push(Finding {
                    severity: "warning", code: "sync-generators-not-run", location,
                    message: "qk does not run sync generators; perform required synchronization separately.".into(),
                });
            }
        }
    }
    // A dependency glob that selects nothing keys nothing, so the cache
    // entries of the tasks reading it are reused whatever their dependencies
    // build. Several globs often share one named input, each for some of the
    // tasks it reaches: only one selecting nothing for any of them is wrong.
    let requests: Vec<qk_taskgraph::Request> = workspace
        .projects
        .iter()
        .flat_map(|(project, definition)| {
            definition
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
    let globs = qk_taskgraph::TaskGraph::build(workspace, &requests)
        .and_then(|graph| qk_cache::dependency_globs(workspace, &graph));
    match globs {
        Ok(globs) => findings.extend(unmatched_globs(&globs)),
        Err(error) => findings.push(Finding {
            severity: "warning",
            code: "dependency-outputs-unchecked",
            location: "workspace".into(),
            message: format!("cannot check dependentTasksOutputFiles globs: {error:#}"),
        }),
    }
    let errors = findings
        .iter()
        .filter(|finding| finding.severity == "error")
        .count();
    let warnings = findings.len() - errors;
    if json {
        crate::print_json(&serde_json::json!({
            "schemaVersion": 1, "projects": workspace.projects.len(),
            "targets": workspace.projects.values().map(|project| project.targets.len()).sum::<usize>(),
            "errors": errors, "warnings": warnings, "findings": findings,
            "acceptedNoopOptions": NOOP_OPTIONS,
            "scope": "Explicit targets only; plugins and inferred targets are not evaluated."
        }))?;
    } else {
        let mut out = io::stdout().lock();
        for finding in &findings {
            writeln!(
                out,
                "{} [{}] {}: {}",
                finding.severity, finding.code, finding.location, finding.message
            )?;
        }
        writeln!(
            out,
            "qk doctor: {errors} errors, {warnings} warnings across {} projects",
            workspace.projects.len()
        )?;
        writeln!(out, "Accepted without effect: {}", NOOP_OPTIONS.join(", "))?;
        writeln!(
            out,
            "Explicit targets only; plugins and inferred targets are not evaluated."
        )?;
    }
    Ok(i32::from(errors > 0 || (strict && warnings > 0)))
}

/// The dependency globs that select nothing for any task of a target, by
/// target name, as `targetDefaults` and named inputs configure them. A glob
/// in a named input reaches every target using it, so the targets sharing a
/// set of such globs are one finding, most tasks first.
fn unmatched_globs(globs: &[qk_cache::DependencyGlob]) -> Vec<Finding> {
    use std::collections::{BTreeMap, BTreeSet};
    let target = |task: &str| task.split(':').nth(1).unwrap_or(task).to_owned();
    let mut uses: BTreeMap<String, BTreeMap<&str, Vec<&qk_cache::DependencyGlob>>> =
        BTreeMap::new();
    for glob in globs {
        uses.entry(target(&glob.task))
            .or_default()
            .entry(&glob.pattern)
            .or_default()
            .push(glob);
    }
    // Each set of globs, with the targets they select nothing for and how
    // many tasks each has.
    let mut sets: BTreeMap<BTreeSet<&str>, Vec<(String, usize)>> = BTreeMap::new();
    for (target, patterns) in uses {
        let tasks = patterns.values().map(Vec::len).max().unwrap_or(0);
        let unmatched: BTreeSet<&str> = patterns
            .into_iter()
            .filter(|(_, uses)| {
                uses.iter().all(|glob| {
                    matches!(
                        glob.selects,
                        qk_cache::Selects::NoDeclaredOutputs | qk_cache::Selects::NoneOf(_)
                    )
                })
            })
            .map(|(pattern, _)| pattern)
            .collect();
        if !unmatched.is_empty() {
            sets.entry(unmatched).or_default().push((target, tasks));
        }
    }
    sets.into_iter()
        .map(|(patterns, mut targets)| {
            targets.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            const SHOWN: usize = 6;
            let shown: Vec<String> = targets
                .iter()
                .take(SHOWN)
                .map(|(target, tasks)| {
                    format!("{target} ({tasks} task{})", if *tasks == 1 { "" } else { "s" })
                })
                .collect();
            let more = if targets.len() > SHOWN {
                format!(" and {} more", targets.len() - SHOWN)
            } else {
                String::new()
            };
            let patterns: Vec<String> =
                patterns.iter().map(|pattern| format!("{pattern:?}")).collect();
            Finding {
                severity: "warning",
                code: "dependency-outputs-unmatched",
                location: format!("dependentTasksOutputFiles {}", patterns.join(", ")),
                message: format!(
                    "select{} nothing for any task of {} target{}: {}{more}. None of those tasks depends on a task whose outputs hold a file to select, so for them the glob{} key{} nothing and their cache entries are reused whatever their dependencies build; where that is wrong, depend on the task producing those files, or fix the glob{}.",
                    if patterns.len() == 1 { "s" } else { "" },
                    targets.len(),
                    if targets.len() == 1 { "" } else { "s" },
                    shown.join(", "),
                    if patterns.len() == 1 { "" } else { "s" },
                    if patterns.len() == 1 { "s" } else { "" },
                    if patterns.len() == 1 { "" } else { "s" },
                ),
            }
        })
        .collect()
}
