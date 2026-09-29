use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

use crate::{Package, Project, Target, WorkspaceConfig, relative_path};

pub(crate) fn project(
    root: &Path,
    directory: &Path,
    config: &WorkspaceConfig,
    project_json: Option<Value>,
    local: Option<Value>,
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
    // Which of Nx's plugins created each target, for `filter.plugin` in
    // target defaults: package.json's, unless project.json gives the target
    // its executor or command or adds it.
    let from_package: std::collections::BTreeSet<String> = targets.keys().cloned().collect();
    let mut from_project = std::collections::BTreeSet::new();
    for file in [&project_json, &local] {
        if let Some(Value::Object(incoming)) = file.as_ref().and_then(|file| file.get("targets")) {
            for (name, target) in incoming {
                if !from_package.contains(name)
                    || target.get("executor").is_some()
                    || target.get("command").is_some()
                {
                    from_project.insert(name.clone());
                }
            }
        }
    }
    if let Some(Value::Object(project)) = project_json {
        overlay(&mut value, &mut targets, project)?;
    }
    if let Some(Value::Object(local)) = local {
        if local.contains_key("name") || local.contains_key("root") {
            bail!("{} cannot set name or root", crate::LOCAL_OVERRIDES);
        }
        overlay(&mut value, &mut targets, local)
            .with_context(|| format!("invalid {}", crate::LOCAL_OVERRIDES))?;
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

    let project_name = value
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let project_tags: Vec<String> = value
        .get("tags")
        .and_then(Value::as_array)
        .map(|tags| {
            tags.iter()
                .filter_map(|tag| tag.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let mut normalized_targets = Map::new();
    for (name, target) in targets {
        let target = normalize_command(target)?;
        let plugin = if from_project.contains(&name) || !from_package.contains(&name) {
            "nx/core/project-json"
        } else {
            "nx/core/package-json"
        };
        let defaults = target_default(
            config,
            &name,
            &target,
            &Filtered {
                project: &project_name,
                tags: &project_tags,
                plugin,
            },
        )
        .with_context(|| format!("invalid targetDefaults for {name:?}"))?;
        let target = match defaults {
            Some(defaults) => merge_target(defaults, target)?,
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

/// Merges one configuration file over the project so far, as Nx merges
/// project.json over package.json: tags as an ordered union, targets through
/// [`merge_target`], named inputs by name, and every other field replaced.
fn overlay(
    value: &mut Map<String, Value>,
    targets: &mut Map<String, Value>,
    mut project: Map<String, Value>,
) -> Result<()> {
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
        merge_targets(targets, explicit)?;
    }
    if let Some(named) = project.remove("namedInputs") {
        let mut merged = object(
            value.remove("namedInputs").unwrap_or_else(|| json!({})),
            "namedInputs",
        )?;
        merged.extend(object(named, "namedInputs")?);
        value.insert("namedInputs".into(), Value::Object(merged));
    }
    value.extend(project);
    Ok(())
}

/// nx.json with `local` merged over it: target defaults as targets merge,
/// named inputs by name, and every other field replaced.
pub(crate) fn workspace(base: Value, local: Value) -> Result<Value> {
    let mut base = object(base, "nx.json")?;
    for (key, value) in object(local, "nx.local.json")? {
        let value = match key.as_str() {
            // Entries merge as targets do; an array of filtered entries
            // replaces what it overrides.
            "targetDefaults" => {
                let mut defaults = object(
                    base.remove(&key).unwrap_or_else(|| json!({})),
                    "targetDefaults",
                )?;
                for (name, local) in object(value, "targetDefaults")? {
                    let merged = match (defaults.remove(&name), local) {
                        (Some(base @ Value::Object(_)), local @ Value::Object(_)) => {
                            merge_target(normalize_command(base)?, normalize_command(local)?)?
                        }
                        (_, local) => local,
                    };
                    defaults.insert(name, merged);
                }
                Value::Object(defaults)
            }
            "namedInputs" => {
                let mut named = object(
                    base.remove(&key).unwrap_or_else(|| json!({})),
                    "namedInputs",
                )?;
                named.extend(object(value, "namedInputs")?);
                Value::Object(named)
            }
            _ => value,
        };
        base.insert(key, value);
    }
    Ok(Value::Object(base))
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

/// What a target default's `filter` is matched against.
struct Filtered<'a> {
    project: &'a str,
    tags: &'a [String],
    plugin: &'a str,
}

/// The target default for a target, as Nx 23 resolves it. Keys are tried in
/// order: the target's executor, its name, then glob keys matching the name,
/// longest first. A key's value is a default or an array of them, each with
/// an optional `filter` on `projects`, `plugin` and `executor`; the first key
/// with an entry whose filter matches wins, and its matching entries merge in
/// order, later winning. An entry naming a different executor from the
/// target's is left out, but its key still wins.
fn target_default(
    config: &WorkspaceConfig,
    name: &str,
    target: &Value,
    context: &Filtered,
) -> Result<Option<Value>> {
    let defaults = &config.target_defaults;
    let executor = target.get("executor").and_then(Value::as_str);
    let mut keys: Vec<&str> = Vec::new();
    if let Some(executor) = executor.filter(|executor| defaults.contains_key(*executor)) {
        keys.push(executor);
    }
    if defaults.contains_key(name) && Some(name) != executor {
        keys.push(name);
    }
    let mut globs: Vec<&str> = defaults
        .keys()
        .map(String::as_str)
        .filter(|key| *key != name && Some(*key) != executor && is_glob(key))
        .filter(|key| {
            globset::Glob::new(key)
                .map(|glob| glob.compile_matcher().is_match(name))
                .unwrap_or(false)
        })
        .collect();
    globs.sort_by_key(|key| std::cmp::Reverse(key.len()));
    keys.extend(globs);
    for key in keys {
        let entries = match &defaults[key] {
            Value::Array(entries) => entries.clone(),
            entry => vec![entry.clone()],
        };
        let mut matched = false;
        let mut merged: Option<Value> = None;
        for entry in entries {
            let mut entry = object(entry, "target default")?;
            let filter = entry.remove("filter");
            if !filter_matches(filter.as_ref(), executor, context)? {
                continue;
            }
            matched = true;
            let entry = normalize_command(Value::Object(entry))?;
            // Nx drops a default that would replace the target's own executor.
            if !compatible(&entry, target) {
                continue;
            }
            merged = Some(match merged {
                Some(merged) => merge_target(merged, entry)?,
                None => entry,
            });
        }
        if matched {
            return Ok(merged);
        }
    }
    Ok(None)
}

/// Nx's `isGlobPattern`, for target default keys.
fn is_glob(key: &str) -> bool {
    key.contains(['*', '?', '[', '{']) || ["!(", "+(", "@("].iter().any(|group| key.contains(group))
}

fn filter_matches(
    filter: Option<&Value>,
    executor: Option<&str>,
    context: &Filtered,
) -> Result<bool> {
    let Some(filter) = filter else {
        return Ok(true);
    };
    let filter = filter.as_object().context("filter must be an object")?;
    for key in filter.keys() {
        if !matches!(key.as_str(), "projects" | "plugin" | "executor") {
            bail!("unknown target default filter {key:?}");
        }
    }
    if let Some(projects) = filter.get("projects") {
        let patterns: Vec<String> = match projects {
            Value::String(pattern) => vec![pattern.clone()],
            other => serde_json::from_value(other.clone())
                .context("filter.projects must be a string or an array")?,
        };
        if !project_matches(&patterns, context)? {
            return Ok(false);
        }
    }
    if let Some(plugin) = filter.get("plugin")
        && plugin.as_str() != Some(context.plugin)
    {
        return Ok(false);
    }
    if let Some(wanted) = filter.get("executor")
        && wanted.as_str() != executor
    {
        return Ok(false);
    }
    Ok(true)
}

/// Whether Nx's project patterns select this project: names and globs,
/// `tag:` globs, and `!` exclusions, starting from every project when the
/// first pattern excludes.
fn project_matches(patterns: &[String], context: &Filtered) -> Result<bool> {
    let matches = |pattern: &str| -> Result<bool> {
        let (tag, glob) = match pattern.strip_prefix("tag:") {
            Some(tag) => (true, tag),
            None => (false, pattern),
        };
        let glob = globset::Glob::new(glob)
            .with_context(|| format!("invalid project pattern {pattern:?}"))?
            .compile_matcher();
        Ok(if tag {
            context.tags.iter().any(|tag| glob.is_match(tag))
        } else {
            glob.is_match(context.project)
        })
    };
    let mut selected = patterns.first().is_some_and(|first| first.starts_with('!'));
    for pattern in patterns {
        match pattern.strip_prefix('!') {
            Some(excluded) => {
                if matches(excluded)? {
                    selected = false;
                }
            }
            None => {
                if matches(pattern)? {
                    selected = true;
                }
            }
        }
    }
    Ok(selected)
}

/// Nx applies a target default unless both name an executor and they differ.
/// Its `isCompatibleTarget` also compares commands and scripts, but Nx calls it
/// here without the target's options, so those never take part.
fn compatible(defaults: &Value, target: &Value) -> bool {
    match (defaults.get("executor"), target.get("executor")) {
        (Some(one), Some(other)) => one == other,
        _ => true,
    }
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
