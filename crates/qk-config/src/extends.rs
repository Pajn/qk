//! nx.json `extends`: the file it names, resolved as Nx resolves it with
//! Node's `require.resolve` from the workspace root.

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use jsonc_parser::JsonValue;

/// A package's `exports`, with objects in the order they are declared, since
/// Node applies the first matching condition.
enum Export {
    Path(String),
    List(Vec<Export>),
    Map(Vec<(String, Export)>),
    Null,
    Other,
}

impl From<JsonValue<'_>> for Export {
    /// Preserve export declaration order and explicit null targets.
    fn from(value: JsonValue<'_>) -> Self {
        match value {
            JsonValue::String(path) => Self::Path(path.into_owned()),
            JsonValue::Array(entries) => Self::List(entries.into_iter().map(Self::from).collect()),
            JsonValue::Object(entries) => Self::Map(
                entries
                    .into_iter()
                    .map(|(key, value)| (key, Self::from(value)))
                    .collect(),
            ),
            JsonValue::Null => Self::Null,
            _ => Self::Other,
        }
    }
}

/// The file `specifier` names: a path relative to the workspace root, or a
/// package subpath such as `nx/presets/npm.json`, found in the `node_modules`
/// of the root or its ancestors through the package's `exports`.
pub fn resolve(root: &Path, specifier: &str) -> Result<Resolved> {
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
        let manifest_path = location.join("package.json");
        let text = match std::fs::read_to_string(&manifest_path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("cannot read {}", manifest_path.display()));
            }
        };
        let mut manifest = match jsonc_parser::parse_to_value(&text, &Default::default())
            .with_context(|| format!("cannot parse {}", manifest_path.display()))?
        {
            Some(JsonValue::Object(manifest)) => manifest,
            _ => jsonc_parser::JsonObject::new(Default::default()),
        };
        let resolved = match manifest.take("exports") {
            Some(exports) => {
                let key = subpath.map_or_else(|| ".".to_owned(), |subpath| format!("./{subpath}"));
                let target = exported(&Export::from(exports), &key).with_context(|| {
                    format!("nx.json extends {specifier:?}, which {package} does not export")
                })?;
                file(&location.join(target))
            }
            None => match subpath {
                Some(subpath) => file(&location.join(subpath)),
                None => manifest
                    .take_string("main")
                    .and_then(|main| {
                        let main = location.join(main.as_ref());
                        file(&main).or_else(|| file(&main.join("index")))
                    })
                    .or_else(|| file(&location.join("index"))),
            },
        };
        return resolved
            .with_context(|| format!("nx.json extends {specifier:?}, which does not exist"));
    }
    bail!("nx.json extends {specifier:?}, but no node_modules contains {package}")
}

/// The path, or the path with `.json` appended, as `require.resolve` tries it,
/// with `.` and `..` segments resolved.
fn file(path: &Path) -> Option<Resolved> {
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
        return Some(Resolved {
            path: path.to_owned(),
            candidate: normal,
        });
    }
    let mut with_extension = path.as_os_str().to_owned();
    with_extension.push(".json");
    let with_extension = PathBuf::from(with_extension);
    with_extension.is_file().then_some(Resolved {
        path: with_extension,
        candidate: normal,
    })
}

/// The selected file and exact path tried before a possible `.json` fallback.
pub struct Resolved {
    pub path: PathBuf,
    pub candidate: PathBuf,
}

/// Where a package's `exports` send `key`: an exact entry, else the pattern
/// with a single `*` whose prefix is longest, then the longest pattern, as
/// Node orders them, with the match substituted.
fn exported(exports: &Export, key: &str) -> Option<String> {
    let Export::Map(entries) = exports else {
        return (key == ".").then(|| target(exports)).flatten().flatten();
    };
    // Conditions alone describe the package's root export.
    if !entries.iter().any(|(name, _)| name.starts_with('.')) {
        return (key == ".").then(|| target(exports)).flatten().flatten();
    }
    if let Some((_, value)) = entries.iter().find(|(name, _)| name == key) {
        return target(value).flatten();
    }
    let (_, value, matched) = entries
        .iter()
        .filter_map(|(pattern, value)| {
            let (prefix, suffix) = pattern.split_once('*')?;
            let middle = key.strip_prefix(prefix)?.strip_suffix(suffix)?;
            Some(((prefix.len(), pattern.len()), value, middle))
        })
        .max_by_key(|(order, _, _)| *order)?;
    Some(target(value)??.replace('*', matched))
}

/// An export target: a path, the first usable entry of an array, or the
/// first declared of the conditions `require.resolve` applies. Outer `None`
/// means unresolved; `Some(None)` blocks later conditions with an explicit null.
fn target(value: &Export) -> Option<Option<String>> {
    match value {
        Export::Path(path) => Some(Some(path.clone())),
        Export::Null => Some(None),
        Export::List(entries) => Some(entries.iter().find_map(|entry| target(entry).flatten())),
        Export::Map(conditions) => conditions.iter().find_map(|(condition, value)| {
            matches!(condition.as_str(), "node" | "require" | "default")
                .then(|| target(value))
                .flatten()
        }),
        Export::Other => None,
    }
}
