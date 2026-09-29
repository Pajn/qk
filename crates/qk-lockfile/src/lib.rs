//! Reads `pnpm-lock.yaml` v9 into importers and the snapshots they install.
//!
//! A snapshot key such as `react-dom@19.2.3(react@19.2.3)` names one
//! installation of a package: its version, the peers it was resolved against
//! and, as `patch_hash=…`, the patch applied to it. What an importer installs
//! is therefore the set of snapshots it reaches, each fingerprinted by its key
//! and the integrity of the package behind it.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug)]
pub struct Lockfile {
    /// Importer path (`.` for the workspace root) to dependency name and reference.
    importers: BTreeMap<String, BTreeMap<String, String>>,
    snapshots: BTreeMap<String, BTreeMap<String, String>>,
    /// Package key (a snapshot key without peers or patch) to its resolution.
    resolutions: BTreeMap<String, Value>,
    global: Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Document {
    lockfile_version: Option<Value>,
    #[serde(default)]
    settings: Value,
    #[serde(default)]
    importers: BTreeMap<String, Importer>,
    #[serde(default)]
    packages: BTreeMap<String, Package>,
    #[serde(default)]
    snapshots: BTreeMap<String, Snapshot>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Importer {
    #[serde(default)]
    dependencies: BTreeMap<String, Specified>,
    #[serde(default)]
    dev_dependencies: BTreeMap<String, Specified>,
    #[serde(default)]
    optional_dependencies: BTreeMap<String, Specified>,
    /// Only in the document pnpm keeps for its own installation.
    config_dependencies: Option<Value>,
    package_manager_dependencies: Option<Value>,
}

#[derive(Deserialize)]
struct Specified {
    version: String,
}

#[derive(Deserialize)]
struct Package {
    #[serde(default)]
    resolution: Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Snapshot {
    #[serde(default)]
    dependencies: BTreeMap<String, String>,
    #[serde(default)]
    optional_dependencies: BTreeMap<String, String>,
}

impl Lockfile {
    pub fn parse(text: &str) -> Result<Self> {
        let mut main = None;
        let mut environment = Vec::new();
        for document in serde_yaml_ng::Deserializer::from_str(text) {
            let value = Value::deserialize(document).context("invalid lockfile YAML")?;
            let document: Document =
                serde_json::from_value(value.clone()).context("invalid lockfile shape")?;
            // pnpm 11 and later write the lock of pnpm itself as a separate
            // document ahead of the workspace's.
            let is_environment = !document.importers.is_empty()
                && document.importers.values().all(|importer| {
                    importer.config_dependencies.is_some()
                        || importer.package_manager_dependencies.is_some()
                });
            if is_environment {
                environment.push(value);
            } else if main.replace(document).is_some() {
                bail!("lockfile has more than one workspace document");
            }
        }
        let document = main.context("lockfile has no workspace document")?;
        let version = document
            .lockfile_version
            .as_ref()
            .and_then(|version| match version {
                Value::String(version) => Some(version.clone()),
                Value::Number(version) => Some(version.to_string()),
                _ => None,
            })
            .context("lockfile has no lockfileVersion")?;
        if version.split('.').next() != Some("9") {
            bail!("unsupported lockfileVersion {version:?}; qk reads version 9");
        }
        let importers = document
            .importers
            .into_iter()
            .map(|(path, importer)| {
                let mut dependencies = BTreeMap::new();
                for section in [
                    importer.dependencies,
                    importer.dev_dependencies,
                    importer.optional_dependencies,
                ] {
                    for (name, specified) in section {
                        dependencies.insert(name, specified.version);
                    }
                }
                (path, dependencies)
            })
            .collect();
        let snapshots = document
            .snapshots
            .into_iter()
            .map(|(key, snapshot)| {
                let mut dependencies = snapshot.dependencies;
                dependencies.extend(snapshot.optional_dependencies);
                (key, dependencies)
            })
            .collect();
        let resolutions = document
            .packages
            .into_iter()
            .map(|(key, package)| (key, package.resolution))
            .collect();
        Ok(Self {
            importers,
            snapshots,
            resolutions,
            global: json!({
                "lockfileVersion": version,
                "settings": document.settings,
                "environment": environment,
            }),
        })
    }

    /// What every installation depends on: the lockfile version, pnpm's
    /// resolution settings and the lock of pnpm itself.
    pub fn global(&self) -> &Value {
        &self.global
    }

    pub fn has_importer(&self, importer: &str) -> bool {
        self.importers.contains_key(importer)
    }

    /// Fingerprints of everything `importer` installs: each direct dependency
    /// by the name it is installed under, and each snapshot reached from them.
    /// Workspace links are left out; they are projects, not packages. `None`
    /// when the lockfile has no such importer.
    pub fn installed(&self, importer: &str) -> Option<BTreeSet<String>> {
        let direct = self.importers.get(importer)?;
        let mut fingerprints = BTreeSet::new();
        let mut pending = Vec::new();
        for (name, reference) in direct {
            if let Some(key) = snapshot_key(name, reference) {
                fingerprints.insert(format!("{name} -> {key}"));
                pending.push(key);
            }
        }
        self.reach(pending, &mut fingerprints);
        Some(fingerprints)
    }

    /// Fingerprints of every installation of the package `name`, whichever
    /// importer installs it, and of everything each one reaches.
    pub fn package(&self, name: &str) -> BTreeSet<String> {
        let keys = self
            .snapshots
            .keys()
            .filter(|key| package_name(key) == name)
            .cloned()
            .collect();
        let mut fingerprints = BTreeSet::new();
        self.reach(keys, &mut fingerprints);
        fingerprints
    }

    fn reach(&self, mut pending: Vec<String>, fingerprints: &mut BTreeSet<String>) {
        let mut seen = BTreeSet::new();
        while let Some(key) = pending.pop() {
            if !seen.insert(key.clone()) {
                continue;
            }
            fingerprints.insert(self.fingerprint(&key));
            for (name, reference) in self.snapshots.get(&key).into_iter().flatten() {
                if let Some(key) = snapshot_key(name, reference) {
                    pending.push(key);
                }
            }
        }
    }

    /// A snapshot's key, the integrity of its package, and its dependencies
    /// with the references they resolve to. A key the lockfile does not list is
    /// fingerprinted as such, so it still changes when the lockfile does.
    fn fingerprint(&self, key: &str) -> String {
        let Some(dependencies) = self.snapshots.get(key) else {
            return json!({"key": key, "missing": true}).to_string();
        };
        let resolution = self.resolutions.get(package_key(key));
        let resolution = resolution
            .and_then(|resolution| resolution.get("integrity"))
            .or(resolution)
            .unwrap_or(&Value::Null);
        json!({"key": key, "resolution": resolution, "dependencies": dependencies}).to_string()
    }
}

/// The snapshot a dependency reference points at, or `None` for a workspace link.
/// References are a version (`4.3.0(react@19.2.3)`), or for an alias the full
/// key of the aliased package (`string-width@4.2.3`).
fn snapshot_key(name: &str, reference: &str) -> Option<String> {
    if reference.starts_with("link:") {
        return None;
    }
    let base = &reference[..reference.find('(').unwrap_or(reference.len())];
    let is_key = !base.contains(':') && base.rfind('@').is_some_and(|at| at > 0);
    Some(if is_key {
        reference.to_owned()
    } else {
        format!("{name}@{reference}")
    })
}

/// The package a snapshot key installs: `@scope/name` or `name`.
fn package_name(key: &str) -> &str {
    let key = package_key(key);
    let start = usize::from(key.starts_with('@'));
    key[start..].find('@').map_or(key, |at| &key[..start + at])
}

/// A snapshot key without its peer and patch suffixes, as `packages` lists it.
fn package_key(key: &str) -> &str {
    &key[..key.find('(').unwrap_or(key.len())]
}
