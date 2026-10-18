use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

use crate::merge;
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
        .shift_remove("includedScripts")
        .map(serde_json::from_value)
        .transpose()
        .context("includedScripts must be an array of script names")?;
    // Each file's targets, merged in order over the target defaults: Nx's
    // package.json plugin, then its project.json plugin, then qk's local file.
    let mut layers: Vec<Map<String, Value>> = Vec::new();
    if let Some(package) = package {
        // As in Nx, a script is no target when project.json gives the target
        // another way to run: a command, or an executor other than
        // run-script running that script.
        let replaced = |script: &str| {
            let Some(target) = project_json
                .as_ref()
                .and_then(|project| project.get("targets")?.get(script))
            else {
                return false;
            };
            let set = |key: &str| target.get(key).is_some_and(merge::truthy);
            set("command")
                || (set("executor")
                    && (target["executor"] != "nx:run-script"
                        || target
                            .get("options")
                            .and_then(|options| options.get("script"))
                            != Some(&Value::String(script.into()))))
        };
        let scripts = included_scripts.unwrap_or_else(|| {
            package
                .scripts
                .keys()
                .filter(|script| !replaced(script))
                .cloned()
                .collect()
        });
        let mut targets = Map::new();
        for script in scripts {
            // Nx creates these targets even when the script is absent.
            targets.insert(
                script.clone(),
                json!({
                    "executor": "nx:run-script", "options": { "script": script }
                }),
            );
        }
        if let Some(explicit) = value.shift_remove("targets") {
            for (name, target) in object(explicit, "targets")? {
                let target = object(target, "target")?;
                // A target saying how it runs replaces the script's; any
                // other merges over it, keeping spreads with nothing beneath
                // for the target defaults.
                let merged = if targets.contains_key(&name)
                    && ["executor", "command"]
                        .iter()
                        .any(|key| target.get(*key).is_some_and(merge::truthy))
                {
                    target
                } else {
                    merge::target(
                        &target,
                        targets.get(&name).and_then(Value::as_object),
                        true,
                        &format!("targets.{name}"),
                    )
                    .with_context(|| format!("invalid target {name:?}"))?
                };
                targets.insert(name, Value::Object(merged));
            }
        }
        layers.push(targets);
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
        if let Some(Value::Array(own)) = value.shift_remove("tags") {
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
    } else {
        value.shift_remove("targets");
    }
    // Which of Nx's plugins created each target, for `filter.plugin` in
    // target defaults: package.json's, unless project.json gives the target
    // its executor or command or adds it.
    let from_package: std::collections::BTreeSet<String> = layers
        .first()
        .map(|targets| targets.keys().cloned().collect())
        .unwrap_or_default();
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
        layers.push(overlay(&mut value, project)?);
    }
    if let Some(Value::Object(local)) = local {
        if local.contains_key("name") || local.contains_key("root") {
            bail!("{} cannot set name or root", crate::LOCAL_OVERRIDES);
        }
        layers.push(
            overlay(&mut value, local)
                .with_context(|| format!("invalid {}", crate::LOCAL_OVERRIDES))?,
        );
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
    value.insert("root".into(), Value::String(relative.clone()));
    let mut named_inputs = serde_json::to_value(&config.named_inputs)?
        .as_object()
        .cloned()
        .expect("named inputs serialize as an object");
    if let Some(local) = value.shift_remove("namedInputs") {
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
    // As in Nx, the target defaults are a layer beneath the files, worked
    // out from the targets the files make together: each matching default
    // takes the executor and command of the target it applies to, so the
    // files merge over it rather than replacing it.
    let mut staged = Map::new();
    for layer in &layers {
        merge_layer(&mut staged, layer, true, &relative, None)?;
    }
    let mut defaults: Vec<(String, usize, Map<String, Value>)> = Vec::new();
    for (name, target) in &staged {
        let target = merge::desugar(object_ref(target, "target")?, &relative)?;
        let plugin = if from_project.contains(name) || !from_package.contains(name) {
            "nx/core/project-json"
        } else {
            "nx/core/package-json"
        };
        let context = Filtered {
            project: &project_name,
            tags: &project_tags,
            plugin,
        };
        let Some((key, entries)) = target_default(config, name, &target, &context)
            .with_context(|| format!("invalid targetDefaults for {name:?}"))?
        else {
            continue;
        };
        for (index, entry) in entries {
            if let Some(entry) = synthetic_default(entry, &target, &relative)
                .with_context(|| format!("invalid targetDefaults for {name:?}"))?
            {
                let mut layer = Map::new();
                layer.insert(name.clone(), Value::Object(entry));
                defaults.push((key.clone(), index, layer));
            }
        }
    }
    // One layer per default, by key and then position, as Nx merges them.
    let mut order: Vec<String> = Vec::new();
    for (key, _, _) in &defaults {
        if !order.contains(key) {
            order.push(key.clone());
        }
    }
    defaults.sort_by_key(|(key, index, _)| (order.iter().position(|k| k == key), *index));
    let mut beneath = Map::new();
    for (_, _, layer) in &defaults {
        merge_layer(&mut beneath, layer, false, &relative, None)?;
    }
    let mut targets = beneath.clone();
    for layer in &layers {
        merge_layer(&mut targets, layer, false, &relative, Some(&beneath))?;
    }
    let mut normalized_targets = Map::new();
    let mut errors = Vec::new();
    for (name, target) in targets {
        let target = normalize_target(object(target, "target")?, &relative, &project_name, &name)
            .with_context(|| format!("invalid target {name:?}"))?;
        if target.get("cache").is_some_and(merge::truthy)
            && target.get("continuous").is_some_and(merge::truthy)
        {
            errors.push(format!(
                "- {name:?} has both \"cache\" and \"continuous\" set to true. Continuous \
                 targets cannot be cached. Please remove the \"cache\" property."
            ));
        }
        // A target that runs nothing is a no-op when it has dependencies,
        // and is left out otherwise.
        if !["executor", "command"]
            .iter()
            .any(|key| target.get(*key).is_some_and(merge::truthy))
        {
            let depends = match target.get("dependsOn") {
                Some(Value::Array(items)) => !items.is_empty(),
                Some(Value::String(text)) => !text.is_empty(),
                _ => false,
            };
            if !depends {
                continue;
            }
            let mut target = target;
            target.insert("executor".into(), json!("nx:noop"));
            insert_target(&mut normalized_targets, name, target)?;
            continue;
        }
        insert_target(&mut normalized_targets, name, target)?;
    }
    if !errors.is_empty() {
        bail!(
            "Errors detected in targets of project {project_name:?}:\n{}",
            errors.join("\n")
        );
    }
    value.insert("targets".into(), Value::Object(normalized_targets));
    let project: Project = serde_json::from_value(Value::Object(value))?;
    if project.name.trim().is_empty() {
        bail!("project name must not be empty");
    }
    Ok(project)
}

fn insert_target(
    targets: &mut Map<String, Value>,
    name: String,
    target: Map<String, Value>,
) -> Result<()> {
    let target = Value::Object(target);
    let _: Target = serde_json::from_value(target.clone())
        .with_context(|| format!("invalid target {name:?}"))?;
    targets.insert(name, target);
    Ok(())
}

/// Nx's `normalizeTarget`: the `command` shorthand expanded, and the
/// `{projectRoot}`, `{projectName}` and `{workspaceRoot}` tokens resolved in
/// every option and configuration, once, as the project graph is built.
fn normalize_target(
    target: Map<String, Value>,
    root: &str,
    name: &str,
    target_name: &str,
) -> Result<Map<String, Value>> {
    let mut target = merge::desugar(&target, root)?;
    let key = format!("{root}:{target_name}");
    let options = match target.get("options") {
        Some(Value::Object(options)) => options.clone(),
        _ => Map::new(),
    };
    if !options.is_empty() || target.contains_key("options") {
        target.insert(
            "options".into(),
            Value::Object(resolve_tokens(&options, root, name, &key)?),
        );
    }
    if let Some(Value::Object(configurations)) = target.get_mut("configurations") {
        for (configuration, options) in configurations.iter_mut() {
            if let Value::Object(fields) = options {
                *fields = resolve_tokens(
                    fields,
                    root,
                    name,
                    &format!("{root}:{target_name}:{configuration}"),
                )?;
            }
        }
    }
    Ok(target)
}

/// Nx's `resolveNxTokensInOptions`: in every string, nested ones included,
/// `{projectRoot}` and `{projectName}` anywhere, and `{workspaceRoot}` only
/// at the start, where it is removed, leaving a workspace-relative path.
fn resolve_tokens(
    object: &Map<String, Value>,
    root: &str,
    name: &str,
    key: &str,
) -> Result<Map<String, Value>> {
    object
        .iter()
        .map(|(option, value)| {
            Ok((
                option.clone(),
                resolve_token_value(value, root, name, key, option)?,
            ))
        })
        .collect()
}

fn resolve_token_value(
    value: &Value,
    root: &str,
    name: &str,
    key: &str,
    option: &str,
) -> Result<Value> {
    Ok(match value {
        Value::String(text) => {
            let text = match text.strip_prefix("{workspaceRoot}") {
                Some(rest) => rest.strip_prefix('/').unwrap_or(rest),
                None => text,
            };
            if text.contains("{workspaceRoot}") {
                bail!(
                    "The {{workspaceRoot}} token is only valid at the beginning of an option. ({key})"
                );
            }
            Value::String(
                text.replace("{projectRoot}", root)
                    .replace("{projectName}", name),
            )
        }
        Value::Object(fields) => Value::Object(resolve_tokens(
            fields,
            root,
            name,
            &format!("{key}.{option}"),
        )?),
        Value::Array(items) => {
            let key = format!("{key}.{option}");
            Value::Array(
                items
                    .iter()
                    .enumerate()
                    .map(|(index, item)| {
                        resolve_token_value(item, root, name, &key, &index.to_string())
                    })
                    .collect::<Result<_>>()?,
            )
        }
        other => other.clone(),
    })
}

/// A target default as Nx lays it beneath a target: it takes the target's
/// executor, and a run-commands target's command, unless it names its own
/// command; and it is left out when it names another executor.
fn synthetic_default(
    entry: Map<String, Value>,
    target: &Map<String, Value>,
    root: &str,
) -> Result<Option<Map<String, Value>>> {
    let authored_executor = entry.contains_key("executor");
    let mut synthetic = merge::desugar(&entry, root)?;
    let executor = target.get("executor");
    let mut shape = Map::new();
    if let Some(executor) = executor {
        shape.insert("executor".into(), executor.clone());
    }
    if !merge::compatible(&shape, &synthetic) {
        return Ok(None);
    }
    let options = target.get("options");
    let identity: Option<Map<String, Value>> = (executor
        .is_some_and(|executor| executor == "nx:run-commands"))
    .then(|| {
        ["command", "commands"]
            .iter()
            .filter_map(|key| Some((key.to_string(), options?.get(*key)?.clone())))
            .collect::<Map<String, Value>>()
    })
    .filter(|identity| !identity.is_empty());
    let names_command = synthetic
        .get("options")
        .and_then(Value::as_object)
        .is_some_and(|options| options.contains_key("command") || options.contains_key("commands"));
    if identity.is_none() || !names_command {
        if let Some(executor) = executor {
            synthetic.insert("executor".into(), executor.clone());
        }
        if let Some(identity) = identity {
            let mut options = synthetic
                .get("options")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            options.shift_remove("command");
            options.shift_remove("commands");
            options.extend(identity);
            synthetic.insert("options".into(), Value::Object(options));
        }
    } else if !authored_executor {
        // The executor the shorthand implies would read as a rival target.
        synthetic.shift_remove("executor");
    }
    Ok(Some(synthetic))
}

/// Merges one file's targets over those so far, as Nx merges a plugin's
/// project into its root map. A glob key merges onto every target it
/// matches, and is a target of its own when it matches none.
///
/// A target that is not [compatible](merge::compatible) with the one so far
/// replaces it. In Nx it also replaces the target defaults beneath, which
/// took the executor and command the files end with, so a default is lost
/// whenever the files change how a target runs between them, for instance
/// when project.json gives a command to a target `includedScripts` makes a
/// script. qk merges such a target over the target defaults alone.
fn merge_layer(
    targets: &mut Map<String, Value>,
    incoming: &Map<String, Value>,
    defer: bool,
    root: &str,
    beneath: Option<&Map<String, Value>>,
) -> Result<()> {
    for (name, target) in incoming {
        let target = merge::desugar(object_ref(target, "target")?, root)
            .with_context(|| format!("invalid target {name:?}"))?;
        let mut matching: Vec<String> = if is_glob(name) {
            targets
                .keys()
                .filter(|existing| glob_matches(name, existing))
                .cloned()
                .collect()
        } else {
            Vec::new()
        };
        if matching.is_empty() {
            matching.push(name.clone());
        }
        for matched in matching {
            let mut base = targets.get(&matched).and_then(Value::as_object);
            if let (Some(existing), Some(beneath)) = (base, beneath)
                && !merge::compatible(existing, &target)
            {
                base = beneath.get(&matched).and_then(Value::as_object);
            }
            let merged = merge::target(&target, base, defer, &format!("targets.{matched}"))
                .with_context(|| format!("invalid target {matched:?}"))?;
            targets.insert(matched, Value::Object(merged));
        }
    }
    Ok(())
}

/// Whether a glob target key, in target defaults or a project, matches a
/// target name.
fn glob_matches(pattern: &str, name: &str) -> bool {
    globset::Glob::new(pattern)
        .map(|glob| glob.compile_matcher().is_match(name))
        .unwrap_or(false)
}

/// Merges one configuration file over the project so far, as Nx merges
/// project.json over package.json: tags as an ordered union, named inputs
/// by name, generators by generator, metadata through [`merge::metadata`],
/// and every other field replaced. Returns the file's targets, which merge
/// separately.
fn overlay(
    value: &mut Map<String, Value>,
    mut project: Map<String, Value>,
) -> Result<Map<String, Value>> {
    if let Some(Value::Array(incoming)) = project.shift_remove("tags") {
        let mut tags = match value.shift_remove("tags") {
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
    let targets = match project.shift_remove("targets") {
        Some(targets) => object(targets, "targets")?,
        None => Map::new(),
    };
    if let Some(named) = project.shift_remove("namedInputs") {
        let mut merged = object(
            value
                .shift_remove("namedInputs")
                .unwrap_or_else(|| json!({})),
            "namedInputs",
        )?;
        merged.extend(object(named, "namedInputs")?);
        value.insert("namedInputs".into(), Value::Object(merged));
    }
    if let Some(Value::Object(generators)) = project.shift_remove("generators") {
        let mut merged = generators.clone();
        if let Some(Value::Object(existing)) = value.get("generators") {
            for (generator, options) in existing {
                let mut options = options.as_object().cloned().unwrap_or_default();
                if let Some(Value::Object(own)) = generators.get(generator) {
                    options.extend(own.clone());
                }
                merged.insert(generator.clone(), Value::Object(options));
            }
        }
        value.insert("generators".into(), Value::Object(merged));
    }
    if let Some(metadata) = project.shift_remove("metadata").filter(merge::truthy) {
        let merged = merge::metadata(&metadata, value.get("metadata"));
        value.insert("metadata".into(), merged);
    }
    value.extend(project);
    Ok(targets)
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
                    base.shift_remove(&key).unwrap_or_else(|| json!({})),
                    "targetDefaults",
                )?;
                for (name, local) in object(value, "targetDefaults")? {
                    let merged = match (defaults.get(&name), &local) {
                        (Some(Value::Object(base)), Value::Object(local)) => {
                            Value::Object(merge::target(
                                local,
                                Some(base),
                                true,
                                &format!("targetDefaults.{name}"),
                            )?)
                        }
                        _ => local,
                    };
                    defaults.insert(name, merged);
                }
                Value::Object(defaults)
            }
            "namedInputs" => {
                let mut named = object(
                    base.shift_remove(&key).unwrap_or_else(|| json!({})),
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

fn object_ref<'a>(value: &'a Value, field: &str) -> Result<&'a Map<String, Value>> {
    value
        .as_object()
        .with_context(|| format!("{field} must be an object"))
}

/// What a target default's `filter` is matched against.
struct Filtered<'a> {
    project: &'a str,
    tags: &'a [String],
    plugin: &'a str,
}

/// A target default entry, with its position in its key's array.
type Entry = (usize, Map<String, Value>);

/// The target defaults for a target, as Nx 24 resolves them. Keys are tried
/// in order: the target's executor, its name, then glob keys matching the
/// name, longest first. A key's value is a default or an array of them, each
/// with an optional `filter` on `projects`, `plugin` and `executor`; the first
/// key with an entry whose filter matches wins, with its matching entries in
/// order, each with its position.
fn target_default(
    config: &WorkspaceConfig,
    name: &str,
    target: &Map<String, Value>,
    context: &Filtered,
) -> Result<Option<(String, Vec<Entry>)>> {
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
        .filter(|key| glob_matches(key, name))
        .collect();
    globs.sort_by_key(|key| std::cmp::Reverse(key.len()));
    keys.extend(globs);
    for key in keys {
        let entries = match &defaults[key] {
            Value::Array(entries) => entries.clone(),
            entry => vec![entry.clone()],
        };
        let mut matched = Vec::new();
        for (index, entry) in entries.into_iter().enumerate() {
            let mut entry = object(entry, "target default")?;
            let filter = entry.shift_remove("filter");
            if filter_matches(filter.as_ref(), executor, context)? {
                matched.push((index, entry));
            }
        }
        if !matched.is_empty() {
            return Ok(Some((key.to_owned(), matched)));
        }
    }
    Ok(None)
}

/// Nx's `isGlobPattern`, for target default keys and dependency targets.
pub fn is_glob(key: &str) -> bool {
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
