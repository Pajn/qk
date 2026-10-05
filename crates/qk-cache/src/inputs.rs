//! Declared task inputs shared by cache keys, revision-change selection and
//! input analysis. Content evaluation remains in `hash`; revision loading and
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

/// A task's selected file and non-file inputs, before cache-key evaluation.
pub struct Resolved {
    /// Workspace-relative files whose content is part of the key.
    pub files: BTreeSet<String>,
    /// Always-on workspace and package configuration, also useful to input analysis.
    pub mandatory: BTreeSet<String>,
    /// Env, runtime and other named values; env and runtime are evaluated only
    /// with a prepared task.
    pub values: BTreeMap<String, Value>,
    /// With a readable pnpm lockfile: the importers whose installs count, and
    /// packages named by `externalDependencies`.
    pub lockfile: Option<(BTreeSet<String>, BTreeSet<String>)>,
    /// `pnpm-workspace.yaml` counts without its resolution keys, which reach
    /// tasks through the lockfile instead.
    pub workspace_file: bool,
    /// Explicit external dependency names, even without a readable lockfile.
    pub external: BTreeSet<String>,
}

impl Resolved {
    /// JSON projections still name whole files for conservative change selection.
    pub fn file_inputs(&self) -> impl Iterator<Item = &str> {
        self.files
            .iter()
            .map(String::as_str)
            .chain(self.json_inputs())
    }

    pub fn json_inputs(&self) -> impl Iterator<Item = &str> {
        self.value_names("json:")
    }

    pub fn is_json_input(&self, path: &str) -> bool {
        self.values.contains_key(&format!("json:{path}"))
    }

    pub fn environment_inputs(&self) -> impl Iterator<Item = &str> {
        self.value_names("env:")
    }

    pub fn runtime_inputs(&self) -> impl Iterator<Item = &str> {
        self.value_names("runtime:")
    }

    /// Each dependency-output pattern and whether it includes transitive tasks.
    pub fn dependency_output_inputs(&self) -> impl Iterator<Item = (&str, bool)> {
        self.values.iter().filter_map(|(name, transitive)| {
            name.strip_prefix("dependentTasksOutputFiles:")
                .map(|pattern| (pattern, transitive == &Value::Bool(true)))
        })
    }

    /// Installation-aware metadata is distinct from ordinary keyed file content.
    pub fn is_resolution_metadata(&self, path: &str) -> bool {
        (path == "pnpm-lock.yaml" && self.lockfile.is_some())
            || (path == "pnpm-workspace.yaml" && self.workspace_file)
    }

    fn value_names<'a>(&'a self, prefix: &'a str) -> impl Iterator<Item = &'a str> {
        self.values
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
            links: BTreeMap::new(),
            directory_links: BTreeMap::new(),
        })
    }

    /// Sorted file reasons, then workspace metadata and installation reasons,
    /// preserving the order used in affected-task reports.
    pub fn reasons(&mut self, inputs: &Resolved) -> Result<Vec<InputChange>> {
        let mut reasons = Vec::new();
        let mut touched = BTreeSet::new();
        for file in inputs.file_inputs() {
            if self.changed.contains(file) {
                touched.insert(file);
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
        for file in touched {
            reasons.push(InputChange::Input {
                file: file.to_owned(),
            });
        }
        if inputs.workspace_file && self.workspace_file_changed {
            reasons.push(InputChange::WorkspaceFile);
        }
        match (&inputs.lockfile, &self.lockfiles) {
            (Some((importers, external)), Some((before, after))) => {
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
                for name in external {
                    if before.package(name) != after.package(name) {
                        reasons.push(InputChange::Package { name: name.clone() });
                    }
                }
            }
            (Some(_), None) if self.changed.contains("pnpm-lock.yaml") => {
                reasons.push(InputChange::Input {
                    file: "pnpm-lock.yaml".into(),
                });
            }
            _ => {}
        }
        Ok(reasons)
    }
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
        assert_eq!(inputs.values["env:PATH"], Value::Null);
        assert_eq!(
            inputs.values["runtime:echo evaluated > runtime-marker"],
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
            each.tasks["app:build"].as_ref().unwrap().values,
            inputs.values
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
        assert!(inputs.is_resolution_metadata("pnpm-lock.yaml"));
        assert!(inputs.is_resolution_metadata("pnpm-workspace.yaml"));
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
