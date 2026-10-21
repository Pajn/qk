//! Task inputs shared by cache keys, revision-change selection and input
//! analysis. Content evaluation remains in `hash`; revision loading and
//! task dependency propagation remain with the caller.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result};
use qk_config::Workspace;
use qk_lockfile::Lockfile;
use qk_taskgraph::TaskGraph;
use serde_json::Value;

use crate::{hash, paths};

/// A task's inputs, before cache-key evaluation: what its declared inputs
/// select, and what qk adds to every task on its own.
pub struct Resolved {
    pub declared: Declared,
    pub inferred: Inferred,
}

/// What a task's `inputs` select, each kept as it says: files by their whole
/// content and JSON inputs by their selected fields, whatever the file is.
#[derive(Default)]
pub struct Declared {
    /// Workspace-relative files whose content is part of the key.
    pub files: BTreeSet<String>,
    /// The files among them an `includeIgnored` fileset read from disk, keyed
    /// by the content they point to, wherever that is, as Nx keys them.
    pub ignored: BTreeSet<String>,
    /// Env, runtime and other named values; env and runtime are evaluated only
    /// with a prepared task.
    pub values: BTreeMap<String, Value>,
    /// The fields JSON inputs select, by file, so that a change can be judged
    /// by them: each distinct selection, when several inputs read one file.
    pub json_fields: BTreeMap<String, Vec<JsonFields>>,
    /// Packages named by `externalDependencies`.
    pub external: BTreeSet<String>,
    /// Whether any `externalDependencies` input is declared, even an empty
    /// one. Without one, Nx hashes every installed package into the task.
    pub external_declared: bool,
    /// The projects whose configuration and root tsconfig paths Nx hashes
    /// into the task: its own, and each project whose inputs it reads, such
    /// as a dependency through `^production` or a project a continuous task
    /// serving it belongs to.
    pub projects: BTreeSet<String>,
    /// The tasks whose declared outputs these inputs read, through
    /// `dependentTasksOutputFiles` or a fileset selecting them. As in Nx, a
    /// change carries from such a task to this one, and from no other.
    pub producers: BTreeSet<String>,
}

/// What qk adds to every task's key on its own, each part by its own rule.
/// A file the declared inputs already select is left out here, since the
/// declared entry counts all of it.
#[derive(Default)]
pub struct Inferred {
    /// Workspace configuration and the task's own manifests, by their whole
    /// content.
    pub files: BTreeSet<String>,
    /// `pnpm-workspace.yaml` without its resolution keys, which reach tasks
    /// through the lockfile instead.
    pub workspace_file: bool,
    /// With a readable pnpm lockfile: the importers whose installs count.
    pub lockfile: Option<BTreeSet<String>>,
    /// Whether qk can read the pnpm lockfile, so that what it records is left
    /// out of manifests, and `externalDependencies` count by their installs.
    pub lockfile_readable: bool,
    /// Dependency manifests, each as its dependents' keys read it. See
    /// [`manifest_for_dependents`].
    pub manifests: BTreeMap<String, Value>,
    /// The root tsconfig, keyed as Nx hashes it: without
    /// `compilerOptions.paths`, and with `selectivelyHashTsConfig` the paths
    /// entries pointing into the task's [`Declared::projects`].
    pub tsconfig: Option<String>,
    /// Files qk would add that the declared inputs already select.
    pub covered: BTreeSet<String>,
}

/// What a `json` input keys of its file.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct JsonFields {
    /// Only these dotted fields, or all of them.
    pub fields: Option<Vec<String>>,
    /// Without these dotted fields.
    pub exclude: Vec<String>,
}

impl Inferred {
    /// Every file this part reads, whichever rule it counts by.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.files
            .iter()
            .map(String::as_str)
            .chain(self.workspace_file.then_some("pnpm-workspace.yaml"))
            .chain(self.lockfile.is_some().then_some("pnpm-lock.yaml"))
            .chain(self.manifests.keys().map(String::as_str))
            .chain(self.tsconfig.as_deref())
    }

    /// Package-manager files keyed by what they resolve to rather than by
    /// their content.
    pub fn is_resolution_metadata(&self, path: &str) -> bool {
        (path == "pnpm-lock.yaml" && self.lockfile.is_some())
            || (path == "pnpm-workspace.yaml" && self.workspace_file)
    }
}

/// The root tsconfig files, in the order Nx picks one.
pub const ROOT_TSCONFIG_FILES: [&str; 2] = ["tsconfig.base.json", "tsconfig.json"];

/// Whether nx.json's runner options set `selectivelyHashTsConfig`, which Nx
/// reads from the runner `--runner` or `NX_TASKS_RUNNER` names when nx.json
/// configures it, or else from `default`.
pub fn selectively_hash_tsconfig(workspace: &Workspace) -> bool {
    let runners = workspace.config.extra.get("tasksRunnerOptions");
    let runner = std::env::var("NX_TASKS_RUNNER")
        .ok()
        .filter(|runner| runners.is_some_and(|runners| runners.get(runner).is_some()))
        .unwrap_or_else(|| "default".into());
    runners
        .and_then(|runners| runners.get(&runner))
        .and_then(|runner| runner.get("options"))
        .and_then(|options| options.get("selectivelyHashTsConfig"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// A root tsconfig split as Nx hashes it: everything but
/// `compilerOptions.paths`, and the paths apart. `None` when it cannot be
/// read as JSON or JSONC, or a paths entry is not a list of strings.
pub struct TsConfig {
    pub rest: Value,
    pub paths: BTreeMap<String, Vec<String>>,
}

impl TsConfig {
    pub fn parse(text: &str) -> Option<Self> {
        let mut rest: Value = jsonc_parser::parse_to_serde_value(text, &Default::default()).ok()?;
        let mut paths = BTreeMap::new();
        if let Some(options) = rest
            .as_object_mut()?
            .get_mut("compilerOptions")
            .and_then(Value::as_object_mut)
            && let Some(removed) = options.remove("paths")
        {
            paths = serde_json::from_value(removed).ok()?;
        }
        Some(Self { rest, paths })
    }

    /// The paths entries Nx's `hash_tsconfig_selectively` keeps for
    /// `projects`: each key with its targets that one of them owns, `owner`
    /// telling a target's project.
    pub fn paths_of<'a>(
        &self,
        projects: &BTreeSet<String>,
        owner: impl Fn(&str) -> Option<&'a str>,
    ) -> BTreeMap<String, Vec<String>> {
        self.paths
            .iter()
            .filter_map(|(key, targets)| {
                let owned: Vec<String> = targets
                    .iter()
                    .filter(|target| owner(target).is_some_and(|owner| projects.contains(owner)))
                    .cloned()
                    .collect();
                (!owned.is_empty()).then(|| (key.clone(), owned))
            })
            .collect()
    }
}

impl Resolved {
    /// The files keyed by their whole content, declared or inferred.
    pub fn whole_files(&self) -> impl Iterator<Item = &str> {
        self.declared
            .files
            .iter()
            .chain(&self.inferred.files)
            .map(String::as_str)
    }

    /// Whether `path` is keyed by its whole content.
    pub fn is_whole_file(&self, path: &str) -> bool {
        self.declared.files.contains(path) || self.inferred.files.contains(path)
    }

    /// Whole files and the files of JSON inputs, for conservative change
    /// selection.
    pub fn file_inputs(&self) -> impl Iterator<Item = &str> {
        self.whole_files().chain(self.json_inputs())
    }

    /// Every file qk adds on its own, including those the declared inputs
    /// also select.
    pub fn always_included(&self) -> BTreeSet<String> {
        self.inferred
            .paths()
            .map(str::to_owned)
            .chain(self.inferred.covered.iter().cloned())
            .collect()
    }

    pub fn json_inputs(&self) -> impl Iterator<Item = &str> {
        self.value_names("json:")
    }

    pub fn is_json_input(&self, path: &str) -> bool {
        self.declared.values.contains_key(&format!("json:{path}"))
    }

    pub fn environment_inputs(&self) -> impl Iterator<Item = &str> {
        self.value_names("env:")
    }

    pub fn runtime_inputs(&self) -> impl Iterator<Item = &str> {
        self.value_names("runtime:")
    }

    /// Each dependency-output pattern and whether it includes transitive tasks.
    pub fn dependency_output_inputs(&self) -> impl Iterator<Item = (&str, bool)> {
        self.declared
            .values
            .iter()
            .filter_map(|(name, transitive)| {
                name.strip_prefix("dependentTasksOutputFiles:")
                    .map(|pattern| (pattern, transitive == &Value::Bool(true)))
            })
    }

    fn value_names<'a>(&'a self, prefix: &'a str) -> impl Iterator<Item = &'a str> {
        self.declared
            .values
            .keys()
            .filter_map(move |name| name.strip_prefix(prefix))
    }
}

/// Each task's resolved inputs, for affected selection. `extra` paths are
/// candidates beside the workspace's files, so a deleted file still matches
/// the inputs that named it. Env and runtime inputs are not evaluated.
pub fn resolve_tasks(
    workspace: &Workspace,
    graph: &TaskGraph,
    extra: &[String],
) -> Result<BTreeMap<String, Resolved>> {
    let cache = paths::cache_location(workspace);
    let snapshot = hash::Snapshot::new(workspace, graph, &cache)?.with_candidates(extra);
    let cancelled = AtomicBool::new(false);
    graph
        .tasks
        .iter()
        .map(|(id, task)| {
            let resolved = hash::resolve(&snapshot, workspace, task, None, &cancelled)
                .with_context(|| format!("cannot resolve the inputs of {id}"))?;
            Ok((id.clone(), resolved))
        })
        .collect()
}

/// Every task's resolved inputs, or why they could not be resolved, with the
/// workspace files they were chosen from.
pub struct Resolution {
    pub candidates: std::collections::BTreeSet<String>,
    pub tasks: BTreeMap<String, std::result::Result<Resolved, String>>,
}

pub fn resolve_each(workspace: &Workspace, graph: &TaskGraph) -> Result<Resolution> {
    let cache = paths::cache_location(workspace);
    let snapshot = hash::Snapshot::new(workspace, graph, &cache)?;
    let cancelled = AtomicBool::new(false);
    let tasks = graph
        .tasks
        .iter()
        .map(|(id, task)| {
            let resolved = hash::resolve(&snapshot, workspace, task, None, &cancelled)
                .map_err(|error| format!("{error:#}"));
            (id.clone(), resolved)
        })
        .collect();
    Ok(Resolution {
        candidates: snapshot.files.clone(),
        tasks,
    })
}

/// A changed input's meaning, independent of revision loading and task reports.
#[derive(Debug, PartialEq, Eq)]
pub enum InputChange {
    Input { file: String },
    Installs { importer: String },
    Package { name: String },
    Lockfile,
    WorkspaceFile,
}

/// Matches one revision change set against any number of resolved tasks, sharing
/// linked-path traversal across tasks. Env and runtime values are not evaluated;
/// the caller supplies lockfile revisions and the workspace-file comparison.
pub struct InputChanges<'a> {
    root: PathBuf,
    files: &'a [String],
    changed: BTreeSet<&'a str>,
    lockfiles: Option<(&'a Lockfile, &'a Lockfile)>,
    workspace_file_changed: bool,
    /// Changed manifests' base and head texts, `None` where absent.
    manifests: BTreeMap<String, (Option<String>, Option<String>)>,
    /// Changed JSON inputs' base and head texts, `None` where absent.
    json: BTreeMap<String, (Option<String>, Option<String>)>,
    links: BTreeMap<String, Vec<String>>,
    directory_links: BTreeMap<PathBuf, Vec<PathBuf>>,
}

impl<'a> InputChanges<'a> {
    /// `files` are workspace-relative changes. `lockfiles` are the readable base
    /// and head revisions when the lockfile changed, or `None` otherwise;
    /// `workspace_file_changed` excludes installation-resolution settings.
    pub fn new(
        root: &Path,
        files: &'a [String],
        lockfiles: Option<(&'a Lockfile, &'a Lockfile)>,
        workspace_file_changed: bool,
    ) -> Result<Self> {
        Ok(Self {
            root: root.canonicalize()?,
            files,
            changed: files.iter().map(String::as_str).collect(),
            lockfiles,
            workspace_file_changed,
            manifests: BTreeMap::new(),
            json: BTreeMap::new(),
            links: BTreeMap::new(),
            directory_links: BTreeMap::new(),
        })
    }

    /// The base and head texts of changed manifests, so that a dependency's
    /// manifest counts by what it decides for dependents. A changed manifest
    /// without them counts as changed.
    pub fn with_manifests(
        mut self,
        manifests: BTreeMap<String, (Option<String>, Option<String>)>,
    ) -> Self {
        self.manifests = manifests;
        self
    }

    /// The base and head texts of changed files that JSON inputs select
    /// fields of, so that such a change counts only when a selected field
    /// changed. A changed JSON input without them counts as changed.
    pub fn with_json(mut self, json: BTreeMap<String, (Option<String>, Option<String>)>) -> Self {
        self.json = json;
        self
    }

    /// Sorted file reasons, then workspace metadata and installation reasons,
    /// preserving the order used in affected-task reports.
    pub fn reasons(&mut self, inputs: &Resolved) -> Result<Vec<InputChange>> {
        let mut reasons = Vec::new();
        let mut touched = BTreeSet::new();
        for file in inputs.file_inputs() {
            if self.changed.contains(file) {
                // A file the inputs also select whole counts by all of it.
                if inputs.is_whole_file(file) || !self.json_unchanged(inputs, file) {
                    touched.insert(file);
                }
                continue;
            }
            if !self.links.contains_key(file) {
                self.links.insert(
                    file.to_owned(),
                    symlink_targets(&self.root, file, &mut self.directory_links)?,
                );
            }
            for target in &self.links[file] {
                touched.extend(
                    self.files
                        .iter()
                        .filter(|changed| {
                            target.is_empty()
                                || **changed == *target
                                || changed
                                    .strip_prefix(target)
                                    .is_some_and(|suffix| suffix.starts_with('/'))
                        })
                        .map(String::as_str),
                );
            }
        }
        let lockfile = inputs.inferred.lockfile_readable;
        for file in inputs.inferred.manifests.keys().map(String::as_str) {
            if !self.changed.contains(file) {
                continue;
            }
            let unchanged = self.manifests.get(file).is_some_and(|(before, after)| {
                let read = |text: &Option<String>| {
                    text.as_deref()
                        .and_then(|text| manifest_for_dependents(text, lockfile))
                };
                let before = read(before);
                before.is_some() && before == read(after)
            });
            if !unchanged {
                touched.insert(file);
            }
        }
        for file in touched {
            reasons.push(InputChange::Input {
                file: file.to_owned(),
            });
        }
        if inputs.inferred.workspace_file && self.workspace_file_changed {
            reasons.push(InputChange::WorkspaceFile);
        }
        // A lockfile keyed whole has counted as an input above.
        let Some(importers) = &inputs.inferred.lockfile else {
            return Ok(reasons);
        };
        match &self.lockfiles {
            Some((before, after)) => {
                if before.global() != after.global() {
                    reasons.push(InputChange::Lockfile);
                }
                for importer in importers {
                    if before.installed(importer) != after.installed(importer) {
                        reasons.push(InputChange::Installs {
                            importer: importer.clone(),
                        });
                    }
                }
                for name in &inputs.declared.external {
                    if before.package(name) != after.package(name) {
                        reasons.push(InputChange::Package { name: name.clone() });
                    }
                }
            }
            None if self.changed.contains("pnpm-lock.yaml") => {
                reasons.push(InputChange::Input {
                    file: "pnpm-lock.yaml".into(),
                });
            }
            None => {}
        }
        Ok(reasons)
    }
}

impl InputChanges<'_> {
    /// Whether `file` is a JSON input whose selected fields are the same at
    /// both revisions.
    fn json_unchanged(&self, inputs: &Resolved, file: &str) -> bool {
        let (Some(selections), Some((before, after))) =
            (inputs.declared.json_fields.get(file), self.json.get(file))
        else {
            return false;
        };
        selections.iter().all(|fields| {
            let read = |text: &Option<String>| {
                text.as_deref()
                    .and_then(|text| hash::json_projection(file, text, fields))
            };
            let before = read(before);
            before.is_some() && before == read(after)
        })
    }
}

/// Manifest fields that cannot change what a dependent task reads or runs: a
/// dependency's scripts run only as its own tasks, and the rest is metadata
/// that resolvers and bundlers do not read. A version can still add or remove
/// a dependency through the range a dependent declares; that changes the
/// project graph, and the dependent's key with it.
const DEPENDENT_IGNORED_FIELDS: &[&str] = &[
    "version",
    "scripts",
    "description",
    "keywords",
    "author",
    "contributors",
    "maintainers",
    "license",
    "homepage",
    "repository",
    "bugs",
    "funding",
    "private",
    "publishConfig",
];

/// Dependency declarations, whose effect reaches dependents through the
/// lockfile's record of what each importer installs.
const DECLARATION_FIELDS: &[&str] = &[
    "dependencies",
    "devDependencies",
    "peerDependencies",
    "peerDependenciesMeta",
    "optionalDependencies",
    "dependenciesMeta",
];

/// A dependency's `package.json` as its dependents' keys read it: without
/// fields that cannot change what they read or run, and, when the lockfile
/// records the installs, without the declarations it resolved. `None` when it
/// cannot be read as a JSON object.
pub fn manifest_for_dependents(text: &str, lockfile: bool) -> Option<Value> {
    let mut value: Value = serde_json::from_str(text).ok()?;
    let object = value.as_object_mut()?;
    object.retain(|key, _| {
        !DEPENDENT_IGNORED_FIELDS.contains(&key.as_str())
            && !(lockfile && DECLARATION_FIELDS.contains(&key.as_str()))
    });
    Some(value)
}

/// `pnpm-workspace.yaml` without the keys that configure resolution, or `None`
/// when it cannot be read as YAML.
pub fn workspace_without_resolution(root: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(root.join("pnpm-workspace.yaml")).ok()?;
    without_resolution(&text)
}

/// A `pnpm-workspace.yaml` text without its resolution keys.
pub fn without_resolution(text: &str) -> Option<Value> {
    let mut value: Value = serde_yaml_ng::from_str(text).ok()?;
    if let Some(object) = value.as_object_mut() {
        object.retain(|key, _| !qk_lockfile::RESOLUTION_KEYS.contains(&key.as_str()));
    }
    Some(value)
}

/// Paths a selected symlink reads, including a target that has been deleted.
fn symlink_targets(
    root: &Path,
    path: &str,
    directory_links: &mut BTreeMap<PathBuf, Vec<PathBuf>>,
) -> Result<Vec<String>> {
    let mut pending = vec![root.join(path)];
    let mut visited = BTreeSet::new();
    let mut targets = BTreeSet::new();
    while let Some(absolute) = pending.pop() {
        if !visited.insert(absolute.clone()) {
            continue;
        }
        let metadata = match std::fs::symlink_metadata(&absolute) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if !metadata.file_type().is_symlink() {
            continue;
        }
        let target = absolute
            .parent()
            .expect("input has a parent")
            .join(std::fs::read_link(&absolute)?);
        let mut normalized = PathBuf::new();
        for component in target.components() {
            match component {
                Component::CurDir => {}
                Component::ParentDir => {
                    normalized.pop();
                }
                component => normalized.push(component.as_os_str()),
            }
        }
        let resolved = target.canonicalize().ok().or_else(|| {
            Some(
                target
                    .parent()?
                    .canonicalize()
                    .ok()?
                    .join(target.file_name()?),
            )
        });
        if normalized.starts_with(root) {
            pending.push(normalized.clone());
        }
        if let Some(resolved) = &resolved
            && resolved.starts_with(root)
            && resolved.is_dir()
        {
            if !directory_links.contains_key(resolved) {
                let mut links = Vec::new();
                for entry in walkdir::WalkDir::new(resolved).follow_links(false) {
                    let entry = entry?;
                    if entry.file_type().is_symlink() {
                        links.push(entry.into_path());
                    }
                }
                directory_links.insert(resolved.clone(), links);
            }
            pending.extend(directory_links[resolved].iter().cloned());
        }
        for target in std::iter::once(normalized).chain(resolved) {
            if let Ok(relative) = target.strip_prefix(root)
                && let Some(relative) = relative.to_str()
            {
                targets.insert(relative.replace('\\', "/"));
            }
        }
    }
    Ok(targets.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use qk_taskgraph::Request;
    use serde_json::json;

    fn workspace(
        inputs: Value,
        lockfile: Option<&str>,
    ) -> (tempfile::TempDir, Workspace, TaskGraph) {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("app")).unwrap();
        std::fs::write(root.path().join("nx.json"), "{}").unwrap();
        std::fs::write(
            root.path().join("app/project.json"),
            json!({
                "name": "app", "targets": {"build": {"command": "echo build", "inputs": inputs}}
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(root.path().join("app/meta.json"), r#"{"version":1}"#).unwrap();
        if let Some(lockfile) = lockfile {
            std::fs::write(root.path().join("pnpm-lock.yaml"), lockfile).unwrap();
            std::fs::write(
                root.path().join("pnpm-workspace.yaml"),
                "packages:\n  - app\n",
            )
            .unwrap();
        }
        let workspace = Workspace::load(root.path()).unwrap();
        let graph = TaskGraph::build(&workspace, &[Request::parse("app:build").unwrap()]).unwrap();
        (root, workspace, graph)
    }

    /// The files `app:build` declares with `inputs`, in a workspace where
    /// `app` depends on `lib` and both hold a few files.
    fn declared(inputs: Value) -> Result<BTreeSet<String>> {
        declared_with(inputs, &[])
    }

    fn declared_with(inputs: Value, extra: &[&str]) -> Result<BTreeSet<String>> {
        let root = tempfile::tempdir().unwrap();
        for file in extra {
            let path = root.path().join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
        std::fs::write(root.path().join(".gitignore"), "dist/\n").unwrap();
        std::fs::write(root.path().join("nx.json"), "{}").unwrap();
        for (path, text) in [
            (
                "app/project.json",
                json!({"name": "app", "implicitDependencies": ["lib"],
                    "namedInputs": {"production": ["{projectRoot}/src/**/*"]},
                    "targets": {"build": {"command": "true", "inputs": inputs}}})
                .to_string(),
            ),
            ("lib/project.json", json!({"name": "lib"}).to_string()),
            ("app/src/main.ts", String::new()),
            ("app/src/app/(group)/page.ts", String::new()),
            ("app/src/app/group/page.ts", String::new()),
            ("app/app/notes.md", String::new()),
            ("app/workspaceRoot/x.ts", String::new()),
            ("lib/src/index.ts", String::new()),
            ("lib/lib.ts", String::new()),
            ("tools/gen.ts", String::new()),
        ] {
            let path = root.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let workspace = Workspace::load(root.path()).unwrap();
        let graph = TaskGraph::build(&workspace, &[Request::parse("app:build").unwrap()]).unwrap();
        let mut resolved = resolve_tasks(&workspace, &graph, &[])?;
        Ok(resolved.remove("app:build").unwrap().declared.files)
    }

    fn files(paths: &[&str]) -> BTreeSet<String> {
        paths.iter().map(|path| (*path).to_owned()).collect()
    }

    #[test]
    fn dependency_filesets_are_written_with_a_caret() {
        assert_eq!(
            declared(json!([
                "^{projectRoot}/src/**/*",
                "^{workspaceRoot}/tools/**/*"
            ]))
            .unwrap(),
            files(&["lib/src/index.ts", "tools/gen.ts"])
        );
        // Only the string spelling takes the caret.
        assert!(declared(json!([{"fileset": "^{projectRoot}/src/**/*"}])).is_err());
    }

    #[test]
    fn filesets_read_the_project_name_and_workspace_root_only_at_the_start() {
        assert_eq!(
            declared(json!(["{workspaceRoot}/{projectName}/src/main.ts"])).unwrap(),
            files(&["app/src/main.ts"])
        );
        // Elsewhere `{workspaceRoot}` is glob text: a group of one name.
        assert_eq!(
            declared(json!(["{projectRoot}/{workspaceRoot}/x.ts"])).unwrap(),
            files(&["app/workspaceRoot/x.ts"])
        );
        // A string that is no named input must be a fileset from a root.
        assert!(declared(json!(["src/**/*"])).is_err());
        assert_eq!(declared(json!(["production"])).unwrap().len(), 3);
    }

    #[test]
    fn a_fileset_naming_a_directory_selects_everything_under_it() {
        let src = files(&[
            "app/src/main.ts",
            "app/src/app/(group)/page.ts",
            "app/src/app/group/page.ts",
        ]);
        for fileset in [
            "{projectRoot}/src",
            "{projectRoot}/src/",
            "{projectRoot}/src/**/*",
        ] {
            assert_eq!(declared(json!([fileset])).unwrap(), src, "{fileset}");
        }
        assert_eq!(
            declared(json!(["{projectRoot}/**/*", "!{projectRoot}/src"])).unwrap(),
            files(&[
                "app/app/notes.md",
                "app/project.json",
                "app/workspaceRoot/x.ts"
            ])
        );
    }

    #[test]
    fn a_backslash_escapes_a_fileset_character() {
        assert_eq!(
            declared(json!([r"{projectRoot}/src/app/\(group\)/**/*"])).unwrap(),
            files(&["app/src/app/(group)/page.ts"])
        );
        assert_eq!(
            declared(json!([r"{projectRoot}/src/app/\(group\)"])).unwrap(),
            files(&["app/src/app/(group)/page.ts"])
        );
    }

    #[test]
    fn json_inputs_can_select_several_files() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("nx.json"), "{}").unwrap();
        for (path, text) in [
            (
                "app/project.json",
                json!({"name": "app", "targets": {"build": {"command": "true",
                    "inputs": [{"json": "{projectRoot}/config/*.json", "fields": ["a"]},
                        {"json": "{workspaceRoot}/shared/**/*.json"}]}}})
                .to_string(),
            ),
            ("app/config/one.json", r#"{"a": 1, "b": 2}"#.to_owned()),
            ("app/config/two.json", r#"{"a": 3}"#.to_owned()),
            ("app/config/notes.txt", String::new()),
            (
                "app/config/nested/project.json",
                json!({"name": "nested"}).to_string(),
            ),
            ("app/config/nested/x.json", "{}".to_owned()),
            ("shared/deep/s.json", r#"{"s": true}"#.to_owned()),
        ] {
            let path = root.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let workspace = Workspace::load(root.path()).unwrap();
        let graph = TaskGraph::build(&workspace, &[Request::parse("app:build").unwrap()]).unwrap();
        let resolved = resolve_tasks(&workspace, &graph, &[]).unwrap();
        let declared = &resolved["app:build"].declared;
        assert_eq!(
            declared.values,
            BTreeMap::from([
                ("json:app/config/one.json".to_owned(), json!({"a": 1})),
                ("json:app/config/two.json".to_owned(), json!({"a": 3})),
                ("json:shared/deep/s.json".to_owned(), json!({"s": true})),
            ])
        );
    }

    #[test]
    fn include_ignored_groups_filter_their_own_positives() {
        let dist = [
            "lib/dist/index.js",
            "lib/dist/index.js.map",
            "app/dist/main.js",
        ];
        let dependencies = |fileset: &str| json!({"fileset": fileset, "includeIgnored": true, "dependencies": true});
        assert_eq!(
            declared_with(
                json!([
                    dependencies("{projectRoot}/dist/**"),
                    dependencies("!{projectRoot}/dist/**/*.map"),
                    {"fileset": "{projectRoot}/dist", "includeIgnored": true},
                ]),
                &dist
            )
            .unwrap(),
            files(&["app/dist/main.js", "lib/dist/index.js"])
        );
        // A negation filters only the group it is resolved with.
        let error = declared_with(
            json!([
                dependencies("{projectRoot}/dist/**"),
                {"fileset": "!{projectRoot}/dist/**/*.map", "includeIgnored": true},
            ]),
            &dist,
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("no positive includeIgnored fileset"),
            "{error:#}"
        );
        // Without the flag, the ignored directory selects nothing.
        assert!(
            declared_with(json!(["{projectRoot}/dist/**"]), &dist)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn always_inputs_must_select_a_project() {
        let error =
            declared(json!([{"input": "default", "projects": ["lib", "missing"], "always": true}]))
                .unwrap_err();
        assert!(
            format!("{error:#}").contains("\"missing\", which matches no project"),
            "{error:#}"
        );
        assert!(declared(json!([{"input": "default", "projects": ["missing"]}])).is_ok());
    }

    #[test]
    fn external_dependencies_cannot_name_a_project() {
        let error = declared(json!([{"externalDependencies": ["lib"]}])).unwrap_err();
        assert!(
            format!("{error:#}").contains("already a dependency"),
            "{error:#}"
        );
    }

    #[test]
    fn declared_inputs_cover_the_files_qk_would_add() {
        let (_root, loaded, graph) = workspace(
            json!([
                "{projectRoot}/meta.json",
                "{workspaceRoot}/nx.json",
                "{workspaceRoot}/pnpm-lock.yaml"
            ]),
            Some(&lockfile("1.0.0", "1.0.0")),
        );
        let resolved = resolve_tasks(&loaded, &graph, &[]).unwrap();
        let inputs = &resolved["app:build"];
        // What the inputs select is declared, and kept whole.
        assert_eq!(
            inputs.declared.files,
            BTreeSet::from(["app/meta.json", "nx.json", "pnpm-lock.yaml"].map(String::from))
        );
        // qk's own copies of those files are left out, as covered.
        assert!(!inputs.inferred.files.contains("nx.json"));
        assert!(inputs.inferred.files.contains("app/project.json"));
        assert_eq!(inputs.inferred.lockfile, None);
        assert_eq!(
            inputs.inferred.covered,
            BTreeSet::from(["nx.json", "pnpm-lock.yaml"].map(String::from))
        );
        assert!(inputs.always_included().contains("nx.json"));
        // Without an input naming it, a readable lockfile counts by installs.
        let (_root, loaded, graph) = workspace(
            json!(["{projectRoot}/meta.json"]),
            Some(&lockfile("1.0.0", "1.0.0")),
        );
        let resolved = resolve_tasks(&loaded, &graph, &[]).unwrap();
        let inputs = &resolved["app:build"];
        assert!(!inputs.declared.files.contains("pnpm-lock.yaml"));
        assert!(inputs.inferred.lockfile.is_some());
        assert!(inputs.inferred.covered.is_empty());
    }

    #[test]
    fn dependency_manifests_change_by_what_they_decide_for_dependents() {
        let root = tempfile::tempdir().unwrap();
        let path = "lib/package.json";
        let inputs = |lockfile: bool| Resolved {
            declared: Declared::default(),
            inferred: Inferred {
                manifests: BTreeMap::from([(path.to_owned(), Value::Null)]),
                lockfile: lockfile.then(BTreeSet::new),
                lockfile_readable: lockfile,
                ..Inferred::default()
            },
        };
        let changed = vec![path.to_owned()];
        let reasons = |before: Option<&str>, after: Option<&str>, lockfile: bool| {
            InputChanges::new(root.path(), &changed, None, false)
                .unwrap()
                .with_manifests(BTreeMap::from([(
                    path.to_owned(),
                    (before.map(String::from), after.map(String::from)),
                )]))
                .reasons(&inputs(lockfile))
                .unwrap()
        };
        let input = [InputChange::Input { file: path.into() }];
        let base = r#"{"name":"lib","version":"1.0.0","dependencies":{"a":"^1"}}"#;
        assert!(
            reasons(
                Some(base),
                Some(r#"{"name":"lib","version":"2.0.0","dependencies":{"a":"^1"}}"#),
                false
            )
            .is_empty()
        );
        assert_eq!(
            reasons(
                Some(base),
                Some(r#"{"name":"lib","version":"1.0.0","dependencies":{"a":"^2"}}"#),
                false
            ),
            input
        );
        assert!(
            reasons(
                Some(base),
                Some(r#"{"name":"lib","version":"1.0.0","dependencies":{"a":"^2"}}"#),
                true
            )
            .is_empty()
        );
        assert_eq!(
            reasons(Some(base), Some(r#"{"name":"other"}"#), true),
            input
        );
        // Added, deleted or unreadable manifests count as changed.
        assert_eq!(reasons(None, Some(base), true), input);
        assert_eq!(reasons(Some(base), None, true), input);
        assert_eq!(reasons(Some("{"), Some("{"), true), input);
        // So does one whose revisions were not read.
        assert_eq!(
            InputChanges::new(root.path(), &changed, None, false)
                .unwrap()
                .reasons(&inputs(true))
                .unwrap(),
            input
        );
    }

    #[test]
    fn shared_declarations_do_not_execute_runtime_inputs_or_read_environment_values() {
        let (root, workspace, graph) = workspace(
            json!([
                {"json": "{projectRoot}/meta.json", "fields": ["version"]},
                {"env": "PATH"}, {"runtime": "echo evaluated > runtime-marker"},
                {"dependentTasksOutputFiles": "**/*.js", "transitive": true}
            ]),
            None,
        );
        let changed = vec!["runtime-marker".into(), "app/meta.json".into()];
        let resolved = resolve_tasks(&workspace, &graph, &changed).unwrap();
        let inputs = &resolved["app:build"];
        assert_eq!(inputs.json_inputs().collect::<Vec<_>>(), ["app/meta.json"]);
        assert_eq!(inputs.environment_inputs().collect::<Vec<_>>(), ["PATH"]);
        assert_eq!(
            inputs.runtime_inputs().collect::<Vec<_>>(),
            ["echo evaluated > runtime-marker"]
        );
        assert_eq!(inputs.declared.values["env:PATH"], Value::Null);
        assert_eq!(
            inputs.declared.values["runtime:echo evaluated > runtime-marker"],
            Value::Null
        );
        assert_eq!(
            inputs.dependency_output_inputs().collect::<Vec<_>>(),
            [("**/*.js", true)]
        );
        assert!(inputs.file_inputs().any(|path| path == "app/meta.json"));
        assert_eq!(
            InputChanges::new(root.path(), &changed, None, false)
                .unwrap()
                .reasons(inputs)
                .unwrap(),
            [InputChange::Input {
                file: "app/meta.json".into()
            }]
        );
        assert!(!root.path().join("runtime-marker").exists());
        let each = resolve_each(&workspace, &graph).unwrap();
        assert_eq!(
            each.tasks["app:build"].as_ref().unwrap().declared.values,
            inputs.declared.values
        );
        assert!(!root.path().join("runtime-marker").exists());
    }

    fn lockfile(app: &str, other: &str) -> String {
        let versions = BTreeSet::from([app, other]);
        let packages: String = versions
            .iter()
            .map(|version| {
                format!("  react@{version}:\n    resolution: {{integrity: sha512-{version}}}\n")
            })
            .collect();
        let snapshots: String = versions
            .iter()
            .map(|version| format!("  react@{version}: {{}}\n"))
            .collect();
        format!(
            "lockfileVersion: '9.0'\nimporters:\n  .: {{}}\n  app:\n    dependencies:\n      react:\n        specifier: '*'\n        version: {app}\n  other:\n    dependencies:\n      react:\n        specifier: '*'\n        version: {other}\npackages:\n{packages}snapshots:\n{snapshots}"
        )
    }

    #[test]
    fn installation_changes_preserve_selectivity_fallback_and_reason_order() {
        let before_text = lockfile("1.0.0", "1.0.0");
        let (root, workspace, graph) = workspace(
            json!([
                {"json": "{projectRoot}/meta.json"}, {"externalDependencies": ["react"]}
            ]),
            Some(&before_text),
        );
        let resolved = resolve_tasks(&workspace, &graph, &[]).unwrap();
        let inputs = &resolved["app:build"];
        assert!(inputs.inferred.is_resolution_metadata("pnpm-lock.yaml"));
        assert!(
            inputs
                .inferred
                .is_resolution_metadata("pnpm-workspace.yaml")
        );
        let before = Lockfile::parse(&before_text).unwrap();
        let unrelated = Lockfile::parse(&lockfile("1.0.0", "2.0.0")).unwrap();
        let changed = vec![
            "pnpm-workspace.yaml".into(),
            "pnpm-lock.yaml".into(),
            "app/meta.json".into(),
        ];
        let mut changes =
            InputChanges::new(root.path(), &changed, Some((&before, &unrelated)), true).unwrap();
        // A named external package sees every importer; the task's own importer
        // is unchanged, so this must not report an installation change for app.
        assert_eq!(
            changes.reasons(inputs).unwrap(),
            [
                InputChange::Input {
                    file: "app/meta.json".into()
                },
                InputChange::WorkspaceFile,
                InputChange::Package {
                    name: "react".into()
                },
            ]
        );
        let after = Lockfile::parse(
            &(lockfile("2.0.0", "2.0.0") + "settings:\n  autoInstallPeers: false\n"),
        )
        .unwrap();
        assert_eq!(
            InputChanges::new(root.path(), &changed, Some((&before, &after)), true)
                .unwrap()
                .reasons(inputs)
                .unwrap(),
            [
                InputChange::Input {
                    file: "app/meta.json".into()
                },
                InputChange::WorkspaceFile,
                InputChange::Lockfile,
                InputChange::Installs {
                    importer: "app".into()
                },
                InputChange::Package {
                    name: "react".into()
                },
            ]
        );
        assert_eq!(
            InputChanges::new(root.path(), &changed, None, false)
                .unwrap()
                .reasons(inputs)
                .unwrap(),
            [
                InputChange::Input {
                    file: "app/meta.json".into()
                },
                InputChange::Input {
                    file: "pnpm-lock.yaml".into()
                },
            ]
        );
    }
}
