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
            let executor = definition.executor.as_deref();
            if !matches!(
                executor,
                Some("nx:run-commands" | "nx:run-script" | "nx:noop")
            ) {
                findings.push(Finding {
                    severity: "error", code: "unsupported-executor", location: location.clone(),
                    message: match executor {
                        Some(executor) => format!("Executor {executor:?} is unsupported; use shell commands, package scripts or nx:noop."),
                        None => "Target has no executor or command.".into(),
                    },
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
