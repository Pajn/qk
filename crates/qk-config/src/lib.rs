//! Workspace discovery and normalization. Configuration is never executed.

mod discovery;
pub mod env;
mod extends;
mod merge;
pub mod minimatch;
mod normalize;
mod overrides;
mod remembered;
mod selector;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub use discovery::find_workspace;
pub use overrides::interpolate_override;
pub use selector::{Matchable, Reader, find_matching_projects, is_glob_pattern};

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
    /// `false` runs the task alone: it waits for every other task, and none
    /// starts while it runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallelism: Option<bool>,
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
    pub version: Option<String>,
    pub description: Option<Value>,
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
    /// Each declared dependency with its range, as Nx reads them: when a name
    /// is in several sections, `dependencies` wins, then `devDependencies`,
    /// then `peerDependencies`, then `optionalDependencies`.
    pub fn dependency_ranges(&self) -> BTreeMap<&str, &str> {
        let mut ranges = BTreeMap::new();
        for section in [
            &self.optional_dependencies,
            &self.peer_dependencies,
            &self.dev_dependencies,
            &self.dependencies,
        ] {
            for (name, range) in section {
                ranges.insert(name.as_str(), range.as_str());
            }
        }
        ranges
    }

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
    /// The file nx.json `extends`, workspace-relative, when it is inside the
    /// workspace.
    pub extended: Option<String>,
    /// The resolved preset path before symlink canonicalization, when in-workspace.
    pub extended_logical: Option<String>,
    /// The exact preset candidate tried before `.json` fallback, even if absent.
    pub extended_candidate: Option<String>,
    /// The nx.json specifier actually resolved, before local overrides are merged.
    pub extended_specifier: Option<String>,
    /// The `nx.local.json` and `project.local.json` files merged in,
    /// workspace-relative.
    pub local_overrides: Vec<String>,
    /// Each project's configuration directory, workspace-relative, `.` for
    /// the root: where its `project.json` and `project.local.json` are read,
    /// which a `root` it gives can move away from its root.
    pub configuration_directories: BTreeMap<String, String>,
    /// What Nx would warn about while reading the workspace.
    pub warnings: Vec<String>,
}

pub use normalize::is_glob;

/// Merged over a project's checked-in configuration, for changes that stay on
/// one machine. It is meant to be ignored by Git; Nx does not read it.
pub const LOCAL_OVERRIDES: &str = "project.local.json";

/// Merged over nx.json in the same way, for the workspace.
pub const LOCAL_WORKSPACE: &str = "nx.local.json";

/// Reads the workspace and project configuration files for
/// [`Workspace::load_with`]: the text of a file by its absolute path, or
/// `None` when it is absent. Projects are read on several threads at once.
pub type ConfigReader<'a> = &'a (dyn Fn(&Path) -> Result<Option<String>> + Sync);

/// The files `load` reads, from disk.
fn read_disk(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("cannot read {}", path.display())),
    }
}

/// The nx.json keys [`WorkspaceConfig::keyed_settings`] leaves out. Two reach
/// a task only through its resolved configuration, which its key holds:
/// `targetDefaults` through its definition, and `namedInputs` through what
/// its inputs select. The rest change no task's work: where changes are
/// compared from, how many tasks run at once and how they are shown,
/// generators, releases, sync, the remote cache, Nx Cloud, the default
/// project, and qk's own affected selection.
pub const UNKEYED_SETTINGS: &[&str] = &[
    "targetDefaults",
    "namedInputs",
    "$schema",
    "extends",
    "defaultBase",
    "affected",
    "parallel",
    "tui",
    "cli",
    "generators",
    "release",
    "sync",
    "s3",
    "nxCloudId",
    "nxCloudAccessToken",
    "nxCloudUrl",
    "nxCloudEncryptionKey",
    "neverConnectToCloud",
    "defaultProject",
    "qk:storeServer",
    "qk:affectedProfiles",
    "qk:reachability",
    "qk:environments",
    "qk:defaultEnv",
    "cacheDirectory",
    "maxCacheSize",
    // No longer supported, as in Nx, and ignored with a warning.
    "implicitDependencies",
];

/// nx.json, the file it extends and nx.local.json together, and where the
/// extended file is.
struct ConfigFiles {
    config: WorkspaceConfig,
    extended: Option<PathBuf>,
    extended_specifier: Option<String>,
    extended_candidate: Option<PathBuf>,
    has_local: bool,
}

/// nx.json with the file it extends and nx.local.json merged in, each read
/// with `read`, under the canonical `root`.
fn read_config(root: &Path, read: ConfigReader) -> Result<ConfigFiles> {
    let json = |path: &Path| -> Result<Option<Value>> {
        read(path)?
            .map(|text| parse_json(&text, &path.display().to_string()))
            .transpose()
    };
    let mut nx_json = json(&root.join("nx.json"))?;
    // As in Nx, the extended file's settings are replaced, not merged, by
    // nx.json's own, and its own `extends` is not followed.
    let mut extended = None;
    let mut extended_specifier = None;
    let mut extended_candidate = None;
    if let Some(Value::Object(own)) = &mut nx_json
        && let Some(specifier) = own.get("extends")
    {
        let specifier = specifier
            .as_str()
            .context("nx.json: extends must be a string")?;
        let exists = |path: &Path| read(path).map(|text| text.is_some());
        let resolved = extends::resolve(root, specifier, &exists)?;
        let path = resolved.path;
        extended_candidate = Some(resolved.candidate);
        extended_specifier = Some(specifier.to_owned());
        let Some(Value::Object(mut base)) = json(&path)? else {
            bail!("cannot read {}", path.display());
        };
        base.extend(std::mem::take(own));
        *own = base;
        extended = Some(path);
    }
    let local = json(&root.join(LOCAL_WORKSPACE))?;
    let has_local = local.is_some();
    let nx_json = match (nx_json, local) {
        (base, Some(local)) => Some(
            normalize::workspace(
                base.unwrap_or_else(|| Value::Object(Default::default())),
                local,
            )
            .with_context(|| format!("invalid {LOCAL_WORKSPACE}"))?,
        ),
        (base, None) => base,
    };
    let config: WorkspaceConfig = match nx_json {
        Some(value) => serde_json::from_value(value).context("invalid nx.json configuration")?,
        None => WorkspaceConfig::default(),
    };
    for (name, defaults) in &config.target_defaults {
        let valid = match defaults {
            Value::Object(_) => true,
            Value::Array(entries) => entries.iter().all(Value::is_object),
            _ => false,
        };
        if !valid {
            bail!("nx.json: targetDefaults.{name} must be an object or an array of objects");
        }
    }
    Ok(ConfigFiles {
        config,
        extended,
        extended_specifier,
        extended_candidate,
        has_local,
    })
}

impl WorkspaceConfig {
    /// The configuration of the workspace at the canonical `root`, with
    /// nx.json, the file it extends and nx.local.json each read with `read`
    /// and merged as [`Workspace::load`] merges them.
    pub fn read(root: &Path, read: ConfigReader) -> Result<Self> {
        Ok(read_config(root, read)?.config)
    }

    /// The settings that can change a task's work other than through its
    /// resolved configuration: every key but [`UNKEYED_SETTINGS`], and of
    /// `cli` its `packageManager`, which decides how package scripts run.
    pub fn keyed_settings(&self) -> Map<String, Value> {
        let mut settings: Map<String, Value> = self
            .extra
            .iter()
            .filter(|(key, _)| !UNKEYED_SETTINGS.contains(&key.as_str()))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        if let Some(manager) = self
            .extra
            .get("cli")
            .and_then(|cli| cli.get("packageManager"))
        {
            settings.insert(
                "cli".into(),
                serde_json::json!({ "packageManager": manager }),
            );
        }
        settings
    }
}

/// The project keys [`Project::keyed_settings`] leaves out, each of which
/// reaches a task only through what its key holds otherwise or changes no
/// task's work: the name is in its id; `targets` reach it through its
/// definition and `namedInputs` through what its inputs select; `tags` and
/// `implicitDependencies` through which projects its inputs and dependencies
/// select; `sourceRoot` through the output paths `{project.sourceRoot}`
/// names; the rest describe the project, or configure qk's affected
/// selection.
pub const UNKEYED_PROJECT_SETTINGS: &[&str] = &[
    "name",
    "targets",
    "namedInputs",
    "tags",
    "implicitDependencies",
    "sourceRoot",
    "metadata",
    "projectType",
    "$schema",
    "generators",
    "release",
    "qk:reachability",
];

impl Project {
    /// The configuration that can change a task of the project other than
    /// through its resolved configuration: every key but
    /// [`UNKEYED_PROJECT_SETTINGS`], such as its `root`.
    pub fn keyed_settings(&self) -> Map<String, Value> {
        let Ok(Value::Object(mut settings)) = serde_json::to_value(self) else {
            unreachable!("a project serializes as an object");
        };
        settings.retain(|key, _| !UNKEYED_PROJECT_SETTINGS.contains(&key.as_str()));
        settings
    }
}

impl Workspace {
    /// The workspace at `root`, read from disk, its projects found by
    /// walking every directory.
    pub fn load(root: &Path) -> Result<Self> {
        Self::load_with(root, &read_disk)
    }

    /// As [`Workspace::load`], but which directories hold a project.json or
    /// a package.json is taken from the record an earlier load saved in
    /// `directory` when it shows that a walk would find the same, and
    /// otherwise found by walking and saved there for the next load, on a
    /// thread of its own that [`finish_remembering`] waits for. The loaded
    /// workspace is what [`Workspace::load`] gives either way. `directory`
    /// belongs to the caller, which removes it to forget the records.
    pub fn load_remembering(root: &Path, directory: &Path) -> Result<Self> {
        Self::load_from(root, &read_disk, Some(directory))
    }

    /// As [`Workspace::load`], with nx.json, the file it extends,
    /// nx.local.json, each project's `project.json` and `project.local.json`
    /// and every package.json read with `read`: the workspace another revision
    /// of those files makes.
    ///
    /// The projects are always found by walking: a record of what is on
    /// disk says nothing of what another revision holds.
    pub fn load_with(root: &Path, read: ConfigReader) -> Result<Self> {
        Self::load_from(root, read, None)
    }

    /// [`Workspace::load_with`], remembering what the walk finds in
    /// `remember`, when given: see [`Workspace::load_remembering`].
    fn load_from(root: &Path, read: ConfigReader, remember: Option<&Path>) -> Result<Self> {
        let root = root
            .canonicalize()
            .map(simplified)
            .with_context(|| format!("cannot open workspace {}", root.display()))?;
        if !root.is_dir() {
            bail!("workspace is not a directory: {}", root.display());
        }
        let ConfigFiles {
            config,
            extended,
            extended_specifier,
            extended_candidate,
            has_local,
        } = read_config(&root, read)?;
        // Every manifest through `read`, so that a workspace of another
        // revision is made of that revision's package.json files too.
        let package_json = |path: &Path| -> Result<Option<Value>> {
            read(path)?
                .map(|text| parse_json(&text, &path.display().to_string()))
                .transpose()
        };
        let mut workspace = Self {
            package_manager: package_json(&root.join("package.json"))?
                .and_then(|value| value.get("packageManager").cloned())
                .map(serde_json::from_value)
                .transpose()
                .context("packageManager must be a string")?,
            root: root.clone(),
            config,
            projects: BTreeMap::new(),
            packages: BTreeMap::new(),
            // Canonical, as a package manager may link the package in: keys
            // read no input through a link.
            extended_logical: extended.as_ref().and_then(|path| {
                path.strip_prefix(&root)
                    .ok()?
                    .to_str()
                    .map(|path| path.replace('\\', "/"))
            }),
            extended_specifier,
            extended_candidate: extended_candidate.and_then(|path| {
                path.strip_prefix(&root)
                    .ok()?
                    .to_str()
                    .map(|path| path.replace('\\', "/"))
            }),
            extended: extended.and_then(|path| {
                let path = path.canonicalize().ok().map(simplified)?;
                let relative = path.strip_prefix(&root).ok()?.to_str()?;
                Some(relative.replace('\\', "/"))
            }),
            local_overrides: if has_local {
                vec![LOCAL_WORKSPACE.to_owned()]
            } else {
                Vec::new()
            },
            configuration_directories: BTreeMap::new(),
            warnings: Vec::new(),
        };
        if workspace
            .config
            .extra
            .get("implicitDependencies")
            .is_some_and(|value| !matches!(value, Value::Null | Value::Bool(false)))
        {
            workspace.warnings.push(
                "nx.json implicitDependencies is no longer supported and is ignored; \
                 use namedInputs instead"
                    .to_owned(),
            );
        }
        // A wrong `cli.packageManager` fails only what runs scripts.
        let package_manager = workspace.package_manager_name().unwrap_or("npm");
        let loaded = load_projects(
            &root,
            &workspace.config,
            package_manager,
            read,
            &mut workspace.warnings,
            remember,
        )?;
        let mut roots = BTreeMap::new();
        for (directory, loaded) in loaded {
            // A project.json `read` holds absent, as at a revision before it
            // was added, makes no project of a directory with nothing else.
            let Some(LoadedProject {
                project,
                package,
                has_local,
            }) = loaded?
            else {
                continue;
            };
            if let Some(previous) = workspace.projects.get(&project.name) {
                bail!(
                    "duplicate project name {:?}: {} and {}",
                    project.name,
                    previous.root,
                    project.root
                );
            }
            // Nx merges every configuration naming one root into one
            // project; qk reads one directory per project.
            if let Some(previous) = roots.insert(project.root.clone(), project.name.clone()) {
                bail!(
                    "projects {previous:?} and {:?} both have root {:?}",
                    project.name,
                    project.root
                );
            }
            if let Some(package) = package? {
                workspace.packages.insert(project.name.clone(), package);
            }
            workspace
                .configuration_directories
                .insert(project.name.clone(), relative_path(&root, &directory)?);
            if has_local {
                let root = match relative_path(&root, &directory)?.as_str() {
                    "." => String::new(),
                    directory => format!("{directory}/"),
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

/// What one directory's manifests make, before it joins the workspace.
struct LoadedProject {
    project: Project,
    /// The package.json of the project's root, or the error reading it,
    /// which [`Workspace::load_with`] reports only once the project is
    /// known not to clash with another, as it was read after those checks.
    package: Result<Option<Package>>,
    /// Whether a `project.local.json` was merged in.
    has_local: bool,
}

/// The project the manifests in `directory` make, each read with `read`, or
/// `None` when it has neither a project.json nor a package.json.
fn read_project(
    root: &Path,
    directory: &Path,
    in_workspaces: bool,
    config: &WorkspaceConfig,
    package_manager: &str,
    read: ConfigReader,
) -> Result<Option<LoadedProject>> {
    let json = |path: &Path| -> Result<Option<Value>> {
        read(path)?
            .map(|text| parse_json(&text, &path.display().to_string()))
            .transpose()
    };
    let package = |path: &Path| -> Result<Option<Package>> {
        json(path)?
            .map(serde_json::from_value)
            .transpose()
            .with_context(|| format!("invalid {}", path.display()))
    };
    let project_json = json(&directory.join("project.json"))?;
    let local = json(&directory.join(LOCAL_OVERRIDES))?;
    let own_package = package(&directory.join("package.json"))?;
    if project_json.is_none() && own_package.is_none() {
        return Ok(None);
    }
    let has_local = local.is_some();
    let project = normalize::project(
        &normalize::Origin {
            root,
            directory,
            config,
            in_workspaces,
            package_manager,
        },
        project_json,
        local,
        own_package.as_ref(),
    )
    .with_context(|| format!("invalid project in {}", directory.display()))?;
    // As in Nx, a project's dependencies, scripts and lockfile importer are
    // its root's package.json, which is not the one beside its
    // configuration when the configuration gives another root.
    let package = if relative_path(root, directory)? == project.root {
        Ok(own_package)
    } else {
        package(&root.join(&project.root).join("package.json"))
    };
    Ok(Some(LoadedProject {
        project,
        package,
        has_local,
    }))
}

/// Each directory holding a project, in order, with what its manifests
/// make.
///
/// A project's manifests and normalization depend on no other project, so
/// each is read on one of a few threads as soon as the walk finds the
/// directory, while the walk, which mostly waits on the filesystem, goes on.
/// Those threads guess whether the workspaces take in the directory's
/// package.json from whether one is there, as the walk does unless an
/// ignore file hides it; once the walk is done, a project read with a guess
/// the walk contradicts, or one the walk found without telling, is read
/// again with what the walk found. So each project is what reading it after
/// the walk makes, and the projects are in the order of their directories,
/// as one thread would have met them, whichever thread finishes first.
///
/// With `remember`, the directories are taken from the record an earlier
/// load saved there when it still holds, and otherwise walked and saved
/// there: see [`remembered`]. The workspaces are read either way, before
/// anything else, so their warnings come first as they did.
type Loaded = Vec<(PathBuf, Result<Option<LoadedProject>>)>;

fn load_projects(
    root: &Path,
    config: &WorkspaceConfig,
    package_manager: &str,
    read: ConfigReader,
    warnings: &mut Vec<String>,
    remember: Option<&Path>,
) -> Result<Loaded> {
    let load = |directory: &Path, in_workspaces: bool| {
        read_project(
            root,
            directory,
            in_workspaces,
            config,
            package_manager,
            read,
        )
    };
    let workspaces = discovery::workspaces(root, read, warnings)?;
    if let Some(directory) = remember
        && let Some(record) = remembered::recall(directory, root)
    {
        // The projects the record names are read at once, in their order,
        // while it is checked. Once it is found not to hold, the projects
        // not yet read are passed over, and what was read is thrown away.
        let projects: Vec<(PathBuf, bool)> =
            discovery::projects(root, record.manifests(), &workspaces)?
                .into_iter()
                .collect();
        let changed = std::sync::atomic::AtomicBool::new(false);
        let (verdict, loaded) = std::thread::scope(|scope| {
            let verdict = scope.spawn(|| {
                let verdict = record.holds();
                if matches!(verdict, remembered::Verdict::Changed) {
                    changed.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                verdict
            });
            let loaded = parallel_map(&projects, |(directory, in_workspaces)| {
                (!changed.load(std::sync::atomic::Ordering::Relaxed))
                    .then(|| load(directory, *in_workspaces))
            });
            (
                verdict
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic)),
                loaded,
            )
        });
        let holds = match verdict {
            remembered::Verdict::Changed => false,
            remembered::Verdict::Holds => true,
            remembered::Verdict::Stamped(stamps) => {
                // Stamped once, the record is compared from now on.
                remember_in(directory, root, record.stamped(stamps));
                true
            }
        };
        if holds {
            return Ok(projects
                .into_iter()
                .map(|(directory, _)| directory)
                .zip(
                    loaded
                        .into_iter()
                        .map(|loaded| loaded.expect("every project is read")),
                )
                .collect());
        }
    }
    // What a walk about to begin is remembered with is looked at first.
    let before = remember.and_then(|_| remembered::before_walk(root));
    let threads = std::thread::available_parallelism()
        .map_or(1, std::num::NonZeroUsize::get)
        .clamp(1, MAX_LOAD_THREADS);
    let (send, receive) = std::sync::mpsc::channel::<(PathBuf, bool)>();
    let receive = std::sync::Mutex::new(receive);
    let (directories, mut early) = std::thread::scope(|scope| {
        let loader = || {
            let mut done = Vec::new();
            loop {
                let next = receive.lock().unwrap().recv();
                let Ok((directory, includes)) = next else {
                    return done;
                };
                let in_workspaces = includes
                    && std::fs::symlink_metadata(directory.join("package.json"))
                        .is_ok_and(|metadata| metadata.is_file());
                let loaded = load(&directory, in_workspaces);
                done.push((directory, (in_workspaces, loaded)));
            }
        };
        // One more loader for every few dozen projects found, so that a
        // small workspace starts no threads it cannot use.
        let loaders = std::sync::Mutex::new(Vec::new());
        let count = std::sync::atomic::AtomicUsize::new(0);
        let found = |directory: &Path, includes: bool| {
            let index = count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if index.is_multiple_of(PROJECTS_PER_THREAD) && index / PROJECTS_PER_THREAD < threads {
                loaders.lock().unwrap().push(scope.spawn(loader));
            }
            // The loaders outlive the walk, so they are there to receive.
            let _ = send.send((directory.to_owned(), includes));
        };
        let walked = discovery::project_directories(root, &workspaces, &found, before.is_some());
        drop(send);
        let mut early = std::collections::HashMap::new();
        for loader in loaders.into_inner().unwrap() {
            let done = loader
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
            early.extend(done);
        }
        walked.map(|walked| (walked, early))
    })?;
    let discovery::Walked {
        projects: directories,
        manifests,
        read: walked,
    } = directories;
    if let (Some(directory), Some((began, outside)), Some(walked)) = (remember, before, walked) {
        remember_in(
            directory,
            root,
            remembered::walked(began, outside, walked, manifests),
        );
    }
    let mut loaded: Vec<(PathBuf, Option<Result<Option<LoadedProject>>>)> = directories
        .iter()
        .map(|(directory, in_workspaces)| {
            let early = early
                .remove(directory)
                .filter(|(guessed, _)| guessed == in_workspaces)
                .map(|(_, loaded)| loaded);
            (directory.clone(), early)
        })
        .collect();
    let missing: Vec<(usize, bool)> = loaded
        .iter()
        .zip(directories.values())
        .enumerate()
        .filter(|(_, ((_, early), _))| early.is_none())
        .map(|(index, (_, in_workspaces))| (index, *in_workspaces))
        .collect();
    let redone = parallel_map(&missing, |(index, in_workspaces)| {
        load(&loaded[*index].0, *in_workspaces)
    });
    for ((index, _), project) in missing.iter().zip(redone) {
        loaded[*index].1 = Some(project);
    }
    Ok(loaded
        .into_iter()
        .map(|(directory, project)| (directory, project.expect("every project is read")))
        .collect())
}

/// How many records [`Workspace::load_remembering`] is saving, and the
/// signal that one is done.
static SAVING: (std::sync::Mutex<usize>, std::sync::Condvar) =
    (std::sync::Mutex::new(0), std::sync::Condvar::new());

/// Saves `record` of the workspace at `root` in `directory` on a thread of
/// its own, which [`finish_remembering`] waits for.
fn remember_in(directory: &Path, root: &Path, record: remembered::Record) {
    let directory = directory.to_owned();
    let root = root.to_owned();
    *SAVING.0.lock().unwrap() += 1;
    std::thread::spawn(move || {
        /// Counts the save done however it ends.
        struct Done;
        impl Drop for Done {
            fn drop(&mut self) {
                *SAVING
                    .0
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) -= 1;
                SAVING.1.notify_all();
            }
        }
        let _done = Done;
        // Saving is only for later loads, which walk when it fails.
        let _ = remembered::save(&directory, &root, &record);
    });
}

/// Waits for the records [`Workspace::load_remembering`] is still saving,
/// for a process about to end, which would otherwise stop them unsaved.
pub fn finish_remembering() {
    let (saving, done) = &SAVING;
    let mut saving = saving.lock().unwrap();
    while *saving > 0 {
        saving = done.wait(saving).unwrap();
    }
}

/// The most threads projects are read on. Reading and normalizing a project
/// takes a fraction of a millisecond, so beyond a few threads the cost of
/// starting them outweighs what they share.
const MAX_LOAD_THREADS: usize = 8;

/// How many projects make it worth starting one more thread to read them.
const PROJECTS_PER_THREAD: usize = 16;

/// `items` mapped by `map` on several threads, the results in the order of
/// `items` whatever order the threads finish them in.
fn parallel_map<T: Sync, R: Send>(items: &[T], map: impl Fn(&T) -> R + Sync) -> Vec<R> {
    // One thread for every few dozen items, so that a small workspace
    // starts no threads it cannot use.
    let threads = std::thread::available_parallelism()
        .map_or(1, std::num::NonZeroUsize::get)
        .min(MAX_LOAD_THREADS)
        .min(items.len().div_ceil(PROJECTS_PER_THREAD));
    if threads <= 1 {
        return items.iter().map(map).collect();
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    let mut results: Vec<Option<R>> = std::iter::repeat_with(|| None).take(items.len()).collect();
    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    let mut done = Vec::new();
                    loop {
                        let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(item) = items.get(index) else {
                            return done;
                        };
                        done.push((index, map(item)));
                    }
                })
            })
            .collect();
        for worker in workers {
            let done = worker
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
            for (index, result) in done {
                results[index] = Some(result);
            }
        }
    });
    results
        .into_iter()
        .map(|result| result.expect("every item is mapped"))
        .collect()
}

impl Workspace {
    /// The project the manifests of `directory`, workspace-relative, would
    /// make, given as text rather than read from disk: a project as it was
    /// at another revision, such as one a change deleted. `None` when
    /// neither manifest is given.
    pub fn project_from_manifests(
        &self,
        directory: &str,
        project_json: Option<&str>,
        package_json: Option<&str>,
    ) -> Result<Option<(Project, Option<Package>)>> {
        if project_json.is_none() && package_json.is_none() {
            return Ok(None);
        }
        let project_json = project_json
            .map(|text| parse_json(text, "project.json"))
            .transpose()?;
        let package: Option<Package> = package_json
            .map(|text| parse_json(text, "package.json"))
            .transpose()?
            .map(serde_json::from_value)
            .transpose()
            .context("invalid package.json")?;
        let package_manager = self.package_manager_name().unwrap_or("npm");
        let project = normalize::project(
            &normalize::Origin {
                root: &self.root,
                directory: &self.root.join(directory),
                config: &self.config,
                in_workspaces: package.is_some(),
                package_manager,
            },
            project_json,
            None,
            package.as_ref(),
        )?;
        Ok(Some((project, package)))
    }

    /// nx.json `pluginsConfig["@nx/js"]`, which Nx's own plugins read.
    fn js_plugin_config(&self) -> Option<&Value> {
        self.config
            .extra
            .get("pluginsConfig")
            .and_then(|config| config.get("@nx/js"))
    }

    /// Whether package.json dependencies make graph edges: unless
    /// `analyzePackageJson` is set to anything but `true`, as Nx reads it.
    /// Nx also turns it off in a workspace without a root package.json; qk,
    /// which reads such workspaces' pnpm-workspace.yaml, does not.
    pub fn analyzes_package_json(&self) -> bool {
        self.js_plugin_config()
            .and_then(|config| config.get("analyzePackageJson"))
            .is_none_or(|analyze| *analyze == Value::Bool(true))
    }

    /// Whether the lockfile describes the installed packages:
    /// `analyzeLockfile`, by default on unless a `lerna.json` stands without
    /// an nx.json, as Nx reads it.
    pub fn analyzes_lockfile(&self) -> bool {
        match self
            .js_plugin_config()
            .and_then(|config| config.get("analyzeLockfile"))
        {
            Some(analyze) => {
                !matches!(analyze, Value::Null | Value::Bool(false))
                    && analyze.as_str() != Some("")
                    && analyze.as_f64() != Some(0.0)
            }
            None => !self.root.join("lerna.json").is_file() || self.root.join("nx.json").is_file(),
        }
    }

    /// The package manager, as Nx detects it: nx.json `cli.packageManager`,
    /// then the lockfile, then the package manager qk was invoked through. qk
    /// also reads the root `packageManager` and `pnpm-workspace.yaml` before
    /// that last step.
    pub fn package_manager_name(&self) -> Result<&'static str> {
        let known = |name: &str| -> Result<&'static str> {
            Ok(match name {
                "npm" => "npm",
                "pnpm" => "pnpm",
                "yarn" => "yarn",
                "bun" => "bun",
                other => bail!("unknown package manager {other:?}"),
            })
        };
        if let Some(name) = self
            .config
            .extra
            .get("cli")
            .and_then(|cli| cli.get("packageManager"))
            .and_then(Value::as_str)
        {
            return known(name);
        }
        let root = &self.root;
        for (file, manager) in [
            ("bun.lockb", "bun"),
            ("bun.lock", "bun"),
            ("yarn.lock", "yarn"),
            ("pnpm-lock.yaml", "pnpm"),
            ("package-lock.json", "npm"),
        ] {
            if root.join(file).is_file() {
                return Ok(manager);
            }
        }
        if let Some(declared) = &self.package_manager {
            return known(declared.split('@').next().unwrap_or(declared));
        }
        if root.join("pnpm-workspace.yaml").is_file() {
            return Ok("pnpm");
        }
        let agent = std::env::var("npm_config_user_agent").unwrap_or_default();
        for manager in ["pnpm", "yarn", "bun"] {
            if agent.starts_with(&format!("{manager}/")) {
                return Ok(manager);
            }
        }
        Ok("npm")
    }
}

/// A canonical Windows path without its verbatim `\\?\` prefix when it is
/// an ordinary drive path. Verbatim paths take no `/` separators, and paths
/// built from the root, such as `{workspaceRoot}/dir`, use them.
fn simplified(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    if let Some(text) = path.to_str()
        && let Some(rest) = text.strip_prefix(r"\\?\")
        && rest.as_bytes().get(1) == Some(&b':')
        && rest.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
    {
        return PathBuf::from(rest);
    }
    path
}

pub(crate) fn read_optional_json(path: &Path) -> Result<Option<Value>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("cannot read {}", path.display())),
    };
    parse_json(&text, &path.display().to_string()).map(Some)
}

/// A JSON object as configuration files hold it: comments allowed, and
/// `//` keys left out. `label` names it in errors.
fn parse_json(text: &str, label: &str) -> Result<Value> {
    let mut value: Value = jsonc_parser::parse_to_serde_value(text, &Default::default())
        .with_context(|| format!("cannot parse {label}"))?;
    if !value.is_object() {
        bail!("{label} must contain a JSON object");
    }
    remove_comment_keys(&mut value);
    Ok(value)
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
