//! Merging one layer of configuration over another, as Nx 24 merges targets:
//! the `"..."` spread token, target compatibility and metadata.

use std::collections::BTreeSet;

use anyhow::{Result, bail};
use serde_json::{Map, Value};

/// In an array, the base's items at its position; as an object key set to
/// `true`, the base's keys at its position.
pub(crate) const SPREAD: &str = "...";

/// Whether an object spreads its base: its `"..."` key is `true`.
fn spreads(value: &Value) -> bool {
    value.get(SPREAD) == Some(&Value::Bool(true))
}

/// The value at `key` of `value` when it is an object.
fn field<'a>(value: Option<&'a Value>, key: &str) -> Option<&'a Value> {
    value.and_then(|value| value.as_object()?.get(key))
}

/// Nx's `INTEGER_LIKE_KEY_PATTERN`.
fn integer_like(key: &str) -> bool {
    key == "0"
        || (key.starts_with(|c: char| ('1'..='9').contains(&c))
            && key.bytes().all(|byte| byte.is_ascii_digit()))
}

/// The key JavaScript enumerates first: array-index keys come before every
/// other key, whatever order they were written in.
fn first_key(object: &Map<String, Value>) -> Option<&str> {
    let index = |key: &str| {
        integer_like(key)
            .then(|| key.parse::<u64>().ok())
            .flatten()
            .filter(|index| *index < u64::from(u32::MAX))
    };
    object
        .keys()
        .filter_map(|key| Some((index(key)?, key.as_str())))
        .min()
        .map(|(_, key)| key)
        .or_else(|| object.keys().next().map(String::as_str))
}

/// Nx rejects an integer-like key beside `"..."`: JavaScript enumerates it
/// first, so where it was written relative to the spread is lost.
fn check_integer_keys(object: &Map<String, Value>, context: &str) -> Result<()> {
    if let Some(key) = first_key(object).filter(|key| integer_like(key)) {
        bail!(
            "{context} uses an integer-like key ({key:?}) alongside the '...' spread token. \
             Integer-like keys are enumerated before other keys regardless of authored order, \
             so their position relative to '...' is ambiguous. Rename the key (e.g. add a \
             non-numeric prefix) or restructure the object."
        );
    }
    Ok(())
}

/// [`check_integer_keys`] for a value the merge drops without merging it.
fn check_dropped(value: Option<&Value>, context: &str) -> Result<()> {
    match value {
        Some(object @ Value::Object(fields)) if spreads(object) => {
            check_integer_keys(fields, context)
        }
        _ => Ok(()),
    }
}

/// Nx's `getMergeValueResult`: `new` replaces `base`, except that an array's
/// `"..."` item and an object's `"...": true` key expand the base where they
/// stand. With `defer`, a spread with no base stays for a later layer.
pub(crate) fn value(
    base: Option<&Value>,
    new: Option<&Value>,
    defer: bool,
    context: &str,
) -> Result<Option<Value>> {
    Ok(match new {
        None => base.cloned(),
        Some(Value::Array(items)) => Some(array(base, items, defer)),
        Some(Value::Object(object)) if spreads(new.unwrap()) => {
            Some(Value::Object(spread_object(base, object, defer, context)?))
        }
        Some(new) => Some(new.clone()),
    })
}

fn array(base: Option<&Value>, items: &[Value], defer: bool) -> Value {
    if !items.iter().any(|item| item.as_str() == Some(SPREAD)) {
        return Value::Array(items.to_vec());
    }
    let mut result = Vec::new();
    for item in items {
        if item.as_str() != Some(SPREAD) || (defer && base.is_none()) {
            result.push(item.clone());
        } else if let Some(Value::Array(base)) = base {
            result.extend(base.iter().cloned());
        }
    }
    Value::Array(result)
}

fn spread_object(
    base: Option<&Value>,
    new: &Map<String, Value>,
    defer: bool,
    context: &str,
) -> Result<Map<String, Value>> {
    check_integer_keys(new, &format!("Object at {context:?}"))?;
    let mut result = Map::new();
    for (key, value) in new {
        if key != SPREAD {
            result.insert(key.clone(), value.clone());
        } else if defer && base.is_none() {
            result.insert(SPREAD.into(), Value::Bool(true));
        } else if let Some(Value::Object(base)) = base {
            // A key written before the spread yields to the base's.
            for (key, value) in base {
                result.insert(key.clone(), value.clone());
            }
        }
    }
    Ok(result)
}

fn object_or_absent<'a>(value: Option<&'a Value>, name: &str) -> Result<Option<&'a Value>> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value @ Value::Object(_)) => Ok(Some(value)),
        Some(_) => bail!("{name} must be an object"),
    }
}

/// Nx's `mergeOptions`, and its `mergeConfigurationValue`: key by key through
/// [`value`], or as one spread object when the new one spreads.
fn keys(new: Option<&Value>, base: Option<&Value>, defer: bool, context: &str) -> Result<Value> {
    if let Some(Value::Object(object)) = new
        && spreads(new.unwrap())
    {
        return Ok(Value::Object(spread_object(base, object, defer, context)?));
    }
    let mut result = Map::new();
    let names: Vec<&String> = unique_keys(&[base, new]);
    for name in names {
        let merged = value(
            field(base, name),
            field(new, name),
            defer,
            &format!("{context}.{name}"),
        )?;
        if let Some(merged) = merged {
            result.insert(name.clone(), merged);
        }
    }
    Ok(Value::Object(result))
}

/// The keys of every object, in order, each once.
fn unique_keys<'a>(objects: &[Option<&'a Value>]) -> Vec<&'a String> {
    let mut seen = BTreeSet::new();
    let mut keys = Vec::new();
    for object in objects.iter().flatten() {
        if let Some(object) = object.as_object() {
            for key in object.keys() {
                if seen.insert(key) {
                    keys.push(key);
                }
            }
        }
    }
    keys
}

/// Nx's `mergeConfigurations`: by configuration name, a name written before
/// a `"..."` key yielding to the base's configuration of that name.
fn configurations(
    new: Option<&Value>,
    base: Option<&Value>,
    defer: bool,
    target: &str,
) -> Result<Value> {
    let empty = Map::new();
    let new_map = new.and_then(Value::as_object).unwrap_or(&empty);
    let base_map = base.and_then(Value::as_object);
    let names: Vec<&String> = new_map.keys().collect();
    let spread = names.iter().position(|name| *name == SPREAD);
    let before: BTreeSet<&String> = spread
        .map(|spread| names[..spread].iter().copied().collect())
        .unwrap_or_default();
    if spread.is_some() {
        check_integer_keys(
            new_map,
            &format!("Configurations at \"{target}.configurations\""),
        )?;
    }
    let preserve = spread.is_some() && defer && base.is_none();
    let mut result = Map::new();
    let process = |name: &String, result: &mut Map<String, Value>| -> Result<()> {
        let context = format!("{target}.configurations.{name}");
        let new = object_or_absent(new_map.get(name), "configuration")?;
        let base = base_map.and_then(|base| base.get(name));
        let merged = if before.contains(name) {
            match base {
                Some(base) => {
                    check_dropped(new, &format!("Object at {context:?}"))?;
                    base.clone()
                }
                None => keys(new, None, defer, &context)?,
            }
        } else {
            keys(
                new,
                object_or_absent(base, "configuration")?,
                defer,
                &context,
            )?
        };
        result.insert(name.clone(), merged);
        Ok(())
    };
    let base_names: Vec<&String> = base_map
        .map(|base| base.keys().filter(|name| *name != SPREAD).collect())
        .unwrap_or_default();
    if spread.is_some() {
        // The base's own configurations stand where the spread does.
        let only_base: Vec<&String> = base_names
            .iter()
            .copied()
            .filter(|name| !new_map.contains_key(*name))
            .collect();
        for name in &names {
            if *name == SPREAD {
                for name in &only_base {
                    process(name, &mut result)?;
                }
                if preserve {
                    result.insert(SPREAD.into(), Value::Bool(true));
                }
            } else {
                process(name, &mut result)?;
            }
        }
    } else {
        for name in base_names.iter().chain(&names) {
            if *name != SPREAD && !result.contains_key(*name) {
                process(name, &mut result)?;
            }
        }
    }
    Ok(Value::Object(result))
}

/// Nx's `mergeUltracache`: replaced unless it spreads, with each array it
/// sets merged against the base's.
fn ultracache(
    new: Option<&Value>,
    base: Option<&Value>,
    defer: bool,
    context: &str,
) -> Result<Option<Value>> {
    let Some(new) = new else {
        return Ok(base.cloned());
    };
    let merged = value(base, Some(new), defer, context)?;
    let (Some(Value::Object(mut resolved)), Some(authored)) = (merged.clone(), new.as_object())
    else {
        return Ok(merged);
    };
    let names: Vec<&String> = authored.keys().collect();
    let before: BTreeSet<&String> = names
        .iter()
        .position(|name| *name == SPREAD)
        .map(|spread| names[..spread].iter().copied().collect())
        .unwrap_or_default();
    let keys: Vec<String> = resolved.keys().cloned().collect();
    for key in keys {
        if !authored.get(&key).is_some_and(Value::is_array)
            || (before.contains(&key) && field(base, &key).is_some())
        {
            continue;
        }
        if let Some(merged) = value(
            field(base, &key),
            authored.get(&key),
            defer,
            &format!("{context}.{key}"),
        )? {
            resolved.insert(key, merged);
        }
    }
    Ok(Some(Value::Object(resolved)))
}

/// Nx's `mergeTargetConfigurations`: `target` over `base`. Every field
/// replaces the base's through [`value`], options and configurations merge by
/// key, and keys written before a `"...": true` key yield to the base. A base
/// that is not [compatible](compatible) is dropped whole.
pub(crate) fn target(
    target: &Map<String, Value>,
    base: Option<&Map<String, Value>>,
    defer: bool,
    id: &str,
) -> Result<Map<String, Value>> {
    let empty = Map::new();
    let base_target = base.unwrap_or(&empty);
    let compatible = compatible(base_target, target);
    let own = |key: &str| matches!(key, "options" | "configurations" | "ultracache" | SPREAD);
    let merge_base: Map<String, Value> = if compatible {
        base_target
            .iter()
            .filter(|(key, _)| !matches!(key.as_str(), "options" | "configurations"))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect()
    } else {
        Map::new()
    };
    let names: Vec<&String> = target.keys().collect();
    let spread = names.iter().position(|name| *name == SPREAD);
    if spread.is_some() {
        check_integer_keys(target, &format!("Target at {id:?}"))?;
    }
    let spreading = compatible && spread.is_some();
    let before: BTreeSet<&String> = match spread {
        Some(spread) if spreading => names[..spread].iter().copied().collect(),
        _ => BTreeSet::new(),
    };
    let preserve = spread.is_some() && defer && base.is_none();
    let mut result = Map::new();
    let process = |key: &String, result: &mut Map<String, Value>| -> Result<()> {
        if own(key) {
            return Ok(());
        }
        let context = format!("{id}.{key}");
        let merged = if before.contains(key) {
            check_dropped(target.get(key), &format!("Object at {context:?}"))?;
            match merge_base.get(key) {
                Some(base) => Some(base.clone()),
                None => value(None, target.get(key), defer, &context)?,
            }
        } else if target.contains_key(key) {
            value(merge_base.get(key), target.get(key), defer, &context)?
        } else {
            merge_base.get(key).cloned()
        };
        if let Some(merged) = merged {
            result.insert(key.clone(), merged);
        }
        Ok(())
    };
    if spreading {
        // The base's own fields stand where the spread does.
        let only_base: Vec<&String> = merge_base
            .keys()
            .filter(|key| !own(key) && !target.contains_key(*key))
            .collect();
        for name in &names {
            if *name == SPREAD {
                for key in &only_base {
                    process(key, &mut result)?;
                }
                if preserve {
                    result.insert(SPREAD.into(), Value::Bool(true));
                }
            } else {
                process(name, &mut result)?;
            }
        }
    } else if compatible {
        let base_keys: Vec<&String> = merge_base.keys().collect();
        let mut seen = BTreeSet::new();
        for key in base_keys.into_iter().chain(names.iter().copied()) {
            if seen.insert(key) {
                process(key, &mut result)?;
            }
        }
    } else {
        for name in &names {
            process(name, &mut result)?;
        }
    }
    let options = object_or_absent(target.get("options"), "options")?;
    let base_options = object_or_absent(base_target.get("options"), "options")?;
    if options.is_some() || base_options.is_some() {
        let base_options = base_options.filter(|_| compatible);
        result.insert(
            "options".into(),
            keys(options, base_options, defer, &format!("{id}.options"))?,
        );
    }
    let configurations_value = object_or_absent(target.get("configurations"), "configurations")?;
    let base_configurations =
        object_or_absent(base_target.get("configurations"), "configurations")?;
    if configurations_value.is_some() || base_configurations.is_some() {
        let base_configurations = base_configurations.filter(|_| compatible);
        result.insert(
            "configurations".into(),
            configurations(configurations_value, base_configurations, defer, id)?,
        );
    }
    if target.contains_key("ultracache") || (compatible && base_target.contains_key("ultracache")) {
        let base_ultracache = base_target.get("ultracache").filter(|_| compatible);
        if let Some(merged) = ultracache(
            target.get("ultracache"),
            base_ultracache,
            defer,
            &format!("{id}.ultracache"),
        )? {
            result.insert("ultracache".into(), merged);
        }
    }
    if let Some(metadata) = target.get("metadata").filter(|value| truthy(value)) {
        result.insert(
            "metadata".into(),
            self::metadata(metadata, base_target.get("metadata")),
        );
    }
    Ok(result)
}

/// JavaScript truthiness.
pub(crate) fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|number| number != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// A value as JavaScript's `Array.prototype.join` writes an item.
fn joined_item(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        Value::Array(items) => items.iter().map(joined_item).collect::<Vec<_>>().join(","),
        Value::Object(_) => "[object Object]".into(),
        other => other.to_string(),
    }
}

/// What a run-commands target runs, as Nx compares it: `command`, else
/// `commands` joined with `&&`.
fn command_identity(target: &Map<String, Value>) -> Option<Value> {
    let options = target.get("options");
    match field(options, "command") {
        Some(command) if !command.is_null() => Some(command.clone()),
        _ => match field(options, "commands") {
            Some(Value::Array(commands)) => Some(Value::String(
                commands
                    .iter()
                    .map(joined_item)
                    .collect::<Vec<_>>()
                    .join(" && "),
            )),
            _ => None,
        },
    }
}

/// Nx's `isCompatibleTarget`: unless one target has no executor, both name
/// the same one, and for run-commands the same command and for run-script
/// the same script, unless one has none. A merge over an incompatible base
/// drops all of it.
///
/// Nx compares commands written as arrays by identity, so two arrays never
/// match; qk compares them by value.
pub(crate) fn compatible(one: &Map<String, Value>, other: &Map<String, Value>) -> bool {
    let (Some(executor), Some(other_executor)) = (
        one.get("executor").filter(|value| truthy(value)),
        other.get("executor").filter(|value| truthy(value)),
    ) else {
        return true;
    };
    if executor != other_executor {
        return false;
    }
    let (one, other) = match executor.as_str() {
        Some("nx:run-commands") => (command_identity(one), command_identity(other)),
        Some("nx:run-script") => (
            field(one.get("options"), "script").cloned(),
            field(other.get("options"), "script").cloned(),
        ),
        _ => return true,
    };
    match (one, other) {
        (Some(one), Some(other)) if truthy(&one) && truthy(&other) => one == other,
        _ => true,
    }
}

/// Nx's `mergeMetadata`: arrays concatenate, objects merge one level deep,
/// concatenating their arrays, and anything else replaces.
pub(crate) fn metadata(new: &Value, existing: Option<&Value>) -> Value {
    let Some(new) = new.as_object() else {
        return new.clone();
    };
    let existing = existing.and_then(Value::as_object);
    let mut result = existing.cloned().unwrap_or_default();
    for (key, value) in new {
        let merged = match (value, existing.and_then(|existing| existing.get(key))) {
            (Value::Array(items), Some(Value::Array(base))) => {
                Value::Array(base.iter().chain(items).cloned().collect())
            }
            (Value::Object(fields), Some(Value::Object(base))) => {
                let mut merged = base.clone();
                for (field, value) in fields {
                    let value = match (value, base.get(field)) {
                        (Value::Array(items), Some(Value::Array(base))) => {
                            Value::Array(base.iter().chain(items).cloned().collect())
                        }
                        (value, _) => value.clone(),
                    };
                    merged.insert(field.clone(), value);
                }
                Value::Object(merged)
            }
            (Value::Null, Some(base @ (Value::Object(_) | Value::Array(_)))) => base.clone(),
            (value, _) => value.clone(),
        };
        result.insert(key.clone(), merged);
    }
    Value::Object(result)
}

/// Nx's `resolveCommandSyntacticSugar`: `command` as run-commands'
/// `options.command`. A target cannot set both it and an executor.
pub(crate) fn desugar(target: &Map<String, Value>, root: &str) -> Result<Map<String, Value>> {
    let Some(command) = target.get("command").filter(|command| truthy(command)) else {
        return Ok(target.clone());
    };
    if target.get("executor").is_some_and(truthy) {
        bail!("Project at {root} should not have executor and command both configured.");
    }
    let mut result = target.clone();
    let command = result
        .shift_remove("command")
        .unwrap_or_else(|| command.clone());
    result.insert("executor".into(), Value::String("nx:run-commands".into()));
    let mut options = match object_or_absent(result.get("options"), "options")? {
        Some(Value::Object(options)) => options.clone(),
        _ => Map::new(),
    };
    options.insert("command".into(), command);
    result.insert("options".into(), Value::Object(options));
    Ok(result)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn merge(new: Value, base: Option<Value>, defer: bool) -> Result<Value> {
        target(
            new.as_object().unwrap(),
            base.as_ref().and_then(Value::as_object),
            defer,
            "targets.build",
        )
        .map(Value::Object)
    }

    #[test]
    fn spreads_in_arrays_expand_the_base_where_they_stand() {
        let merged = merge(
            json!({"inputs": ["mine", "...", "last"], "dependsOn": ["..."]}),
            Some(json!({"inputs": ["a", "b"], "dependsOn": ["^build"]})),
            false,
        )
        .unwrap();
        assert_eq!(merged["inputs"], json!(["mine", "a", "b", "last"]));
        assert_eq!(merged["dependsOn"], json!(["^build"]));
        // With nothing beneath, the spread stays for a later layer, or goes.
        let deferred = merge(json!({"outputs": ["...", "x"]}), None, true).unwrap();
        assert_eq!(deferred["outputs"], json!(["...", "x"]));
        let resolved = merge(json!({"outputs": ["...", "x"]}), None, false).unwrap();
        assert_eq!(resolved["outputs"], json!(["x"]));
    }

    #[test]
    fn keys_before_a_spread_yield_to_the_base() {
        let merged = merge(
            json!({
                "cache": false, "...": true, "parallelism": false,
                "options": {"mode": "mine", "...": true, "extra": 1},
                "configurations": {"prod": {"a": 1}, "...": true, "dev": {"...": true, "b": 2}}
            }),
            Some(json!({
                "cache": true, "parallelism": true, "continuous": false,
                "options": {"mode": "base", "env": {"A": "1"}},
                "configurations": {"prod": {"base": true}, "dev": {"c": 3}, "ci": {}}
            })),
            false,
        )
        .unwrap();
        assert_eq!(merged["cache"], true);
        assert_eq!(merged["parallelism"], false);
        assert_eq!(merged["continuous"], false);
        assert_eq!(
            merged["options"],
            json!({"mode": "base", "env": {"A": "1"}, "extra": 1})
        );
        assert_eq!(merged["configurations"]["prod"], json!({"base": true}));
        assert_eq!(merged["configurations"]["dev"], json!({"c": 3, "b": 2}));
        assert_eq!(merged["configurations"]["ci"], json!({}));
        // An option's own object spreads too.
        let merged = merge(
            json!({"options": {"env": {"B": "2", "...": true}}}),
            Some(json!({"options": {"env": {"A": "1", "B": "1"}}})),
            false,
        )
        .unwrap();
        assert_eq!(merged["options"]["env"], json!({"B": "1", "A": "1"}));
    }

    #[test]
    fn integer_like_keys_beside_a_spread_are_rejected() {
        for new in [
            json!({"...": true, "0": 1}),
            json!({"options": {"env": {"...": true, "12": "x"}}}),
            json!({"configurations": {"...": true, "1": {}}}),
        ] {
            let error = merge(new, Some(json!({})), false).unwrap_err().to_string();
            assert!(error.contains("integer-like key"), "{error}");
        }
        // Above the largest array index, JavaScript keeps the written order.
        merge(
            json!({"...": true, "4294967295": 1}),
            Some(json!({})),
            false,
        )
        .unwrap();
    }

    #[test]
    fn an_incompatible_base_is_dropped_whole() {
        let base = json!({
            "executor": "nx:run-commands", "cache": true, "dependsOn": ["^build"],
            "options": {"command": "tsc", "cwd": "x"}, "configurations": {"prod": {}}
        });
        let merged = merge(
            json!({"executor": "nx:run-commands", "options": {"command": "vite build"}}),
            Some(base.clone()),
            false,
        )
        .unwrap();
        assert_eq!(
            merged,
            json!({
                "executor": "nx:run-commands", "options": {"command": "vite build"},
                "configurations": {}
            })
        );
        // The same command, or none on one side, merges.
        let merged = merge(json!({"options": {"cwd": "y"}}), Some(base), false).unwrap();
        assert_eq!(merged["cache"], true);
        assert_eq!(merged["options"], json!({"command": "tsc", "cwd": "y"}));
        let scripts =
            |script: &str| json!({"executor": "nx:run-script", "options": {"script": script}});
        assert!(!compatible(
            scripts("a").as_object().unwrap(),
            scripts("b").as_object().unwrap()
        ));
        let commands = |commands: Value| json!({"executor": "nx:run-commands", "options": {"commands": commands}});
        assert!(compatible(
            commands(json!(["a", "b"])).as_object().unwrap(),
            json!({"executor": "nx:run-commands", "options": {"command": "a && b"}})
                .as_object()
                .unwrap()
        ));
    }

    #[test]
    fn metadata_merges_one_level_deep_concatenating_arrays() {
        let merged = metadata(
            &json!({"technologies": ["vite"], "help": {"options": ["b"], "url": "new"}, "x": 1}),
            Some(
                &json!({"technologies": ["node"], "help": {"options": ["a"], "command": "c"}, "y": 2}),
            ),
        );
        assert_eq!(
            merged,
            json!({
                "technologies": ["node", "vite"],
                "help": {"options": ["a", "b"], "command": "c", "url": "new"},
                "y": 2, "x": 1
            })
        );
    }

    #[test]
    fn command_cannot_be_combined_with_an_executor() {
        let target = json!({"executor": "nx:run-commands", "command": "x"});
        let error = desugar(target.as_object().unwrap(), "app")
            .unwrap_err()
            .to_string();
        assert!(error.contains("executor and command both"), "{error}");
        let target = json!({"command": "x", "options": {"cwd": "a"}});
        assert_eq!(
            Value::Object(desugar(target.as_object().unwrap(), "app").unwrap()),
            json!({"options": {"cwd": "a", "command": "x"}, "executor": "nx:run-commands"})
        );
    }
}
