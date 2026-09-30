//! nx.json `extends`: the file it names, resolved as Nx resolves it with
//! Node's `require.resolve` from the workspace root.

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::Value;

/// The file `specifier` names: a path relative to the workspace root, or a
/// package subpath such as `nx/presets/npm.json`, found in the `node_modules`
/// of the root or its ancestors through the package's `exports`.
pub fn resolve(root: &Path, specifier: &str) -> Result<PathBuf> {
    let relative = specifier.starts_with("./") || specifier.starts_with("../");
    if relative || Path::new(specifier).is_absolute() {
        return file(&root.join(specifier))
            .with_context(|| format!("nx.json extends {specifier:?}, which does not exist"));
    }
    // A scoped package's name has a slash of its own.
    let scoped = usize::from(specifier.starts_with('@'));
    let (package, subpath) = match specifier.match_indices('/').nth(scoped) {
        Some((index, _)) => (&specifier[..index], Some(&specifier[index + 1..])),
        None => (specifier, None),
    };
    for directory in root.ancestors() {
        let location = directory.join("node_modules").join(package);
        if !location.is_dir() {
            continue;
        }
        let manifest = crate::read_optional_json(&location.join("package.json"))?;
        let target = match manifest
            .as_ref()
            .and_then(|manifest| manifest.get("exports"))
        {
            Some(exports) => {
                let key = subpath.map_or_else(|| ".".to_owned(), |subpath| format!("./{subpath}"));
                exported(exports, &key).with_context(|| {
                    format!("nx.json extends {specifier:?}, which {package} does not export")
                })?
            }
            None => subpath.unwrap_or("package.json").to_owned(),
        };
        return file(&location.join(target))
            .with_context(|| format!("nx.json extends {specifier:?}, which does not exist"));
    }
    bail!("nx.json extends {specifier:?}, but no node_modules contains {package}")
}

/// The path, or the path with `.json` appended, as `require.resolve` tries it,
/// with `.` and `..` segments resolved.
fn file(path: &Path) -> Option<PathBuf> {
    let mut normal = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normal.pop();
            }
            other => normal.push(other),
        }
    }
    let path = normal.as_path();
    if path.is_file() {
        return Some(path.to_owned());
    }
    let mut with_extension = path.as_os_str().to_owned();
    with_extension.push(".json");
    let with_extension = PathBuf::from(with_extension);
    with_extension.is_file().then_some(with_extension)
}

/// Where a package's `exports` send `key`: an exact entry, else the pattern
/// with a single `*` whose prefix is longest, then the longest pattern, as
/// Node orders them, with the match substituted.
fn exported(exports: &Value, key: &str) -> Option<String> {
    let Value::Object(map) = exports else {
        return (key == ".").then(|| target(exports)).flatten();
    };
    // Conditions alone describe the package's root export.
    if !map.keys().any(|name| name.starts_with('.')) {
        return (key == ".").then(|| target(exports)).flatten();
    }
    if let Some(value) = map.get(key) {
        return target(value);
    }
    let (_, value, matched) = map
        .iter()
        .filter_map(|(pattern, value)| {
            let (prefix, suffix) = pattern.split_once('*')?;
            let middle = key.strip_prefix(prefix)?.strip_suffix(suffix)?;
            Some(((prefix.len(), pattern.len()), value, middle))
        })
        .max_by_key(|(order, _, _)| *order)?;
    Some(target(value)?.replace('*', matched))
}

/// An export target: a path, the first usable entry of an array, or the
/// entry for the conditions `require.resolve` applies.
fn target(value: &Value) -> Option<String> {
    match value {
        Value::String(path) => Some(path.clone()),
        Value::Array(entries) => entries.iter().find_map(target),
        Value::Object(conditions) => ["node", "require", "default"]
            .iter()
            .find_map(|condition| conditions.get(*condition).and_then(target)),
        _ => None,
    }
}
