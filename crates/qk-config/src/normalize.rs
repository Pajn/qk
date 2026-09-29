use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

use crate::{Package, Project, Target, WorkspaceConfig, relative_path};

pub(crate) fn project(
    root: &Path,
    directory: &Path,
    config: &WorkspaceConfig,
    project_json: Option<Value>,
    package: Option<&Package>,
) -> Result<Project> {
    let relative = relative_path(root, directory)?;
    let mut value = match package.and_then(|p| p.nx.as_ref()) {
        Some(Value::Object(nx)) => nx.clone(),
        Some(_) => bail!("package.json nx must be an object"),
        None => Map::new(),
    };
    let included_scripts: Option<Vec<String>> = value
        .remove("includedScripts")
        .map(serde_json::from_value)
        .transpose()
        .context("includedScripts must be an array of script names")?;
    let mut targets = Map::new();
    if let Some(package) = package {
        let scripts = included_scripts.unwrap_or_else(|| package.scripts.keys().cloned().collect());
        for script in scripts {
            // Nx creates these targets even when the script is absent. Explicit
            // targets in package.json or project.json can replace them below.
            targets.insert(
                script.clone(),
                json!({
                    "executor": "nx:run-script", "options": { "script": script }
                }),
            );
        }
    }
    if let Some(explicit) = value.remove("targets") {
        merge_targets(&mut targets, explicit)?;
    }
    if let Some(package) = package {
        // Nx's package.json plugin: `npm:` tags first, then the package's nx tags.
        let mut tags = vec![Value::String(
            if package.private {
                "npm:private"
            } else {
                "npm:public"
            }
            .into(),
        )];
        tags.extend(
            package
                .keywords
                .iter()
                .map(|keyword| Value::String(format!("npm:{keyword}"))),
        );
        if let Some(Value::Array(own)) = value.remove("tags") {
            tags.extend(own);
        }
        value.insert("tags".into(), Value::Array(tags));
        // workspaceLayout decides projectType over the package's own nx value.
        let layout = config.extra.get("workspaceLayout");
        let apps = layout
            .and_then(|layout| layout.get("appsDir"))
            .and_then(Value::as_str);
        let libs = layout
            .and_then(|layout| layout.get("libsDir"))
            .and_then(Value::as_str);
        // Plain string prefixes, as in Nx.
        if apps.is_some_and(|apps| Some(apps) != libs && relative.starts_with(apps)) {
            value.insert("projectType".into(), json!("application"));
        } else if libs.is_some_and(|libs| relative.starts_with(libs)) {
            value.insert("projectType".into(), json!("library"));
        }
    }
    if let Some(Value::Object(mut project)) = project_json {
        // Nx merges tags as an ordered union rather than replacing them.
        if let Some(Value::Array(incoming)) = project.remove("tags") {
            let mut tags = match value.remove("tags") {
                Some(Value::Array(tags)) => tags,
                _ => Vec::new(),
            };
            for tag in incoming {
                if !tags.contains(&tag) {
                    tags.push(tag);
                }
            }
            value.insert("tags".into(), Value::Array(tags));
        }
        if let Some(explicit) = project.remove("targets") {
            merge_targets(&mut targets, explicit)?;
        }
        // Project named inputs override package-level definitions by name.
        if let Some(named) = project.remove("namedInputs") {
            let mut merged = object(
                value.remove("namedInputs").unwrap_or_else(|| json!({})),
                "namedInputs",
            )?;
            merged.extend(object(named, "namedInputs")?);
            value.insert("namedInputs".into(), Value::Object(merged));
        }
        value.extend(project);
    }
    let inferred_name = package.and_then(|p| p.name.clone()).unwrap_or_else(|| {
        if relative == "." {
            directory
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        } else {
            relative.replace('/', "-")
        }
    });
    value.entry("name").or_insert(Value::String(inferred_name));
    if let Some(configured_root) = value.get("root") {
        let configured = configured_root.as_str().context("root must be a string")?;
        if configured != relative && !(relative == "." && configured.is_empty()) {
            bail!("root {configured:?} does not match configuration directory {relative:?}");
        }
    }
    value.insert("root".into(), Value::String(relative));
    let mut named_inputs = serde_json::to_value(&config.named_inputs)?
        .as_object()
        .cloned()
        .expect("named inputs serialize as an object");
    if let Some(local) = value.remove("namedInputs") {
        named_inputs.extend(object(local, "namedInputs")?);
    }
    value.insert("namedInputs".into(), Value::Object(named_inputs));

    let mut normalized_targets = Map::new();
    for (name, target) in targets {
        let target = normalize_command(target)?;
        // Executor defaults take precedence over name defaults; never combine both.
        let defaults = target
            .get("executor")
            .and_then(Value::as_str)
            .and_then(|executor| config.target_defaults.get(executor))
            .or_else(|| config.target_defaults.get(&name));
        let target = match defaults {
            Some(defaults) => merge_target(normalize_command(defaults.clone())?, target)?,
            None => target,
        };
        let target = normalize_command(target)?;
        let _: Target = serde_json::from_value(target.clone())
            .with_context(|| format!("invalid target {name:?}"))?;
        normalized_targets.insert(name, target);
    }
    value.insert("targets".into(), Value::Object(normalized_targets));
    let project: Project = serde_json::from_value(Value::Object(value))?;
    if project.name.trim().is_empty() {
        bail!("project name must not be empty");
    }
    Ok(project)
}

fn object(value: Value, field: &str) -> Result<Map<String, Value>> {
    match value {
        Value::Object(object) => Ok(object),
        _ => bail!("{field} must be an object"),
    }
}

fn merge_targets(targets: &mut Map<String, Value>, incoming: Value) -> Result<()> {
    for (name, target) in object(incoming, "targets")? {
        let target = normalize_command(target)?;
        let merged = match targets.remove(&name) {
            Some(previous) => merge_target(previous, target)?,
            None => target,
        };
        targets.insert(name, merged);
    }
    Ok(())
}

/// Merge options by key and configurations by configuration name and option key.
/// Arrays and all other target fields are replaced, never concatenated.
fn merge_target(base: Value, overlay: Value) -> Result<Value> {
    let mut base = object(base, "target")?;
    let overlay = object(overlay, "target")?;
    if let (Some(previous), Some(next)) = (base.get("executor"), overlay.get("executor"))
        && previous != next
    {
        base.remove("options");
        base.remove("configurations");
    }
    for (key, value) in overlay {
        let value = if matches!(key.as_str(), "options" | "configurations") {
            let mut merged = object(base.remove(&key).unwrap_or_else(|| json!({})), &key)?;
            for (name, value) in object(value, &key)? {
                let value = if key == "configurations" {
                    let mut configuration = object(
                        merged.remove(&name).unwrap_or_else(|| json!({})),
                        "configuration",
                    )?;
                    configuration.extend(object(value, "configuration")?);
                    Value::Object(configuration)
                } else {
                    value
                };
                merged.insert(name, value);
            }
            Value::Object(merged)
        } else {
            value
        };
        base.insert(key, value);
    }
    Ok(Value::Object(base))
}

fn normalize_command(target: Value) -> Result<Value> {
    let mut target = object(target, "target")?;
    if let Some(command) = target.remove("command") {
        if !command.is_string() {
            bail!("target command must be a string");
        }
        if target
            .get("executor")
            .is_some_and(|executor| executor != "nx:run-commands")
        {
            bail!("target command cannot be combined with an executor other than nx:run-commands");
        }
        target.insert("executor".into(), json!("nx:run-commands"));
        let mut options = object(
            target.remove("options").unwrap_or_else(|| json!({})),
            "options",
        )?;
        options.insert("command".into(), command);
        target.insert("options".into(), Value::Object(options));
    }
    Ok(Value::Object(target))
}
