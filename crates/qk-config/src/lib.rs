//! Workspace discovery and normalization. Configuration is never executed.

mod discovery;
mod normalize;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub use discovery::find_workspace;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Target {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub executor: Option<String>,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub options: Map<String, Value>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub configurations: BTreeMap<String, Map<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_configuration: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inputs: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outputs: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub depends_on: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub continuous: Option<bool>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    pub name: String,
    /// Workspace-relative path with forward slashes; the root project uses `.`.
    pub root: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_type: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub implicit_dependencies: Vec<String>,
    #[serde(default)]
    pub named_inputs: BTreeMap<String, Vec<Value>>,
    #[serde(default)]
    pub targets: BTreeMap<String, Target>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceConfig {
    #[serde(default)]
    pub named_inputs: BTreeMap<String, Vec<Value>>,
    #[serde(default)]
    pub target_defaults: BTreeMap<String, Value>,
    pub default_base: Option<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Package {
    pub name: Option<String>,
    #[serde(default)]
    pub scripts: BTreeMap<String, String>,
    #[serde(default)]
    pub dependencies: BTreeMap<String, String>,
    #[serde(default)]
    pub dev_dependencies: BTreeMap<String, String>,
    #[serde(default)]
    pub peer_dependencies: BTreeMap<String, String>,
    #[serde(default)]
    pub optional_dependencies: BTreeMap<String, String>,
    pub nx: Option<Value>,
    #[serde(default)]
    pub private: bool,
    #[serde(default)]
    pub keywords: Vec<String>,
    // Entry points; Nx infers a library from any of them.
    pub exports: Option<Value>,
    pub main: Option<Value>,
    pub module: Option<Value>,
    pub bin: Option<Value>,
}

impl Package {
    pub fn dependency_names(&self) -> impl Iterator<Item = &String> {
        self.dependencies
            .keys()
            .chain(self.dev_dependencies.keys())
            .chain(self.peer_dependencies.keys())
            .chain(self.optional_dependencies.keys())
    }
}

#[derive(Debug)]
pub struct Workspace {
    pub root: PathBuf,
    pub package_manager: Option<String>,
    pub config: WorkspaceConfig,
    pub projects: BTreeMap<String, Project>,
    /// Package manifests indexed by normalized project name.
    pub packages: BTreeMap<String, Package>,
    /// The `project.local.json` files merged into projects, workspace-relative.
    pub local_overrides: Vec<String>,
}

/// Merged over a project's checked-in configuration, for changes that stay on
/// one machine. It is meant to be ignored by Git; Nx does not read it.
pub const LOCAL_OVERRIDES: &str = "project.local.json";

impl Workspace {
    pub fn load(root: &Path) -> Result<Self> {
        let root = root
            .canonicalize()
            .with_context(|| format!("cannot open workspace {}", root.display()))?;
        if !root.is_dir() {
            bail!("workspace is not a directory: {}", root.display());
        }
        let config: WorkspaceConfig = match read_optional_json(&root.join("nx.json"))? {
            Some(value) => {
                serde_json::from_value(value).context("invalid nx.json configuration")?
            }
            None => WorkspaceConfig::default(),
        };
        for (name, defaults) in &config.target_defaults {
            if !defaults.is_object() {
                bail!(
                    "nx.json: targetDefaults.{name} must be an object; filtered defaults are not supported yet"
                );
            }
        }
        let mut workspace = Self {
            package_manager: read_optional_json(&root.join("package.json"))?
                .and_then(|value| value.get("packageManager").cloned())
                .map(serde_json::from_value)
                .transpose()
                .context("packageManager must be a string")?,
            root: root.clone(),
            config,
            projects: BTreeMap::new(),
            packages: BTreeMap::new(),
            local_overrides: Vec::new(),
        };
        for directory in discovery::project_directories(&root)? {
            let project_json = read_optional_json(&directory.join("project.json"))?;
            let local = read_optional_json(&directory.join(LOCAL_OVERRIDES))?;
            let package: Option<Package> = read_optional_json(&directory.join("package.json"))?
                .map(serde_json::from_value)
                .transpose()
                .with_context(|| format!("invalid {}", directory.join("package.json").display()))?;
            let project = normalize::project(
                &root,
                &directory,
                &workspace.config,
                project_json,
                local.clone(),
                package.as_ref(),
            )
            .with_context(|| format!("invalid project in {}", directory.display()))?;
            if let Some(previous) = workspace.projects.get(&project.name) {
                bail!(
                    "duplicate project name {:?}: {} and {}",
                    project.name,
                    previous.root,
                    project.root
                );
            }
            if let Some(package) = package {
                workspace.packages.insert(project.name.clone(), package);
            }
            if local.is_some() {
                let root = if project.root == "." {
                    String::new()
                } else {
                    format!("{}/", project.root)
                };
                workspace
                    .local_overrides
                    .push(format!("{root}{LOCAL_OVERRIDES}"));
            }
            workspace.projects.insert(project.name.clone(), project);
        }
        Ok(workspace)
    }
}

pub(crate) fn read_optional_json(path: &Path) -> Result<Option<Value>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("cannot read {}", path.display())),
    };
    let mut value: Value = jsonc_parser::parse_to_serde_value(&text, &Default::default())
        .with_context(|| format!("cannot parse {}", path.display()))?;
    if !value.is_object() {
        bail!("{} must contain a JSON object", path.display());
    }
    remove_comment_keys(&mut value);
    Ok(Some(value))
}

fn remove_comment_keys(value: &mut Value) {
    match value {
        Value::Object(object) => {
            object.remove("//");
            object.values_mut().for_each(remove_comment_keys);
        }
        Value::Array(array) => array.iter_mut().for_each(remove_comment_keys),
        _ => {}
    }
}

pub(crate) fn relative_path(root: &Path, path: &Path) -> Result<String> {
    let relative = path.strip_prefix(root)?;
    if relative.as_os_str().is_empty() {
        return Ok(".".into());
    }
    relative
        .components()
        .map(|component| {
            component
                .as_os_str()
                .to_str()
                .map(str::to_owned)
                .context("project paths must be valid UTF-8")
        })
        .collect::<Result<Vec<_>>>()
        .map(|parts| parts.join("/"))
}
