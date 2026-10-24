//! What a change to a project's `project.json` or `project.local.json`
//! affects.
//!
//! Nx hashes `project.json` as a file input wherever a glob selects it, as
//! `{projectRoot}/**/*` does, so any change to it, even a comment, affects
//! every task of its project and of the projects reading it through `^`
//! inputs. qk counts it by what it changes, as the cache keys do: each
//! project's configuration resolved with the manifest at both revisions.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::Result;
use qk_config::{Project, Workspace};
use serde_json::Value;

use crate::Changes;

/// The changed manifests counted by what they change.
#[derive(Default)]
pub(crate) struct ManifestChange {
    /// Each manifest of a project at both revisions whose base revision was
    /// read. Any other changed manifest, such as one the change added or
    /// deleted, counts by the rules for files.
    pub(crate) files: BTreeSet<String>,
    /// The workspace the base revision's manifests make, with the head's
    /// other files; `None` when `files` is empty.
    pub(crate) base: Option<Box<Workspace>>,
}

impl ManifestChange {
    pub(crate) fn new(workspace: &Workspace, changes: &Changes) -> Self {
        let Some(base) = changes.base.as_deref().filter(|_| !changes.whole) else {
            return Self::default();
        };
        let roots = crate::ProjectRoots::new(workspace);
        let mut texts = BTreeMap::new();
        for file in &changes.files {
            if !crate::is_project_file(file) || !workspace.root.join(file).is_file() {
                continue;
            }
            // Only a project's own manifest configures it.
            let directory = file
                .rsplit_once('/')
                .map_or(".", |(directory, _)| directory);
            if roots
                .owner(file)
                .is_none_or(|owner| workspace.projects[owner].root != directory)
            {
                continue;
            }
            if let Some(text) = changes.read(file, Some(base)) {
                texts.insert(file.clone(), text);
            }
        }
        if texts.is_empty() {
            return Self::default();
        }
        let read = |path: &Path| -> Result<Option<String>> {
            let relative = path
                .strip_prefix(&workspace.root)
                .ok()
                .and_then(Path::to_str)
                .map(|path| path.replace('\\', "/"));
            if let Some(text) = relative.and_then(|relative| texts.get(&relative)) {
                return Ok(Some(text.clone()));
            }
            match std::fs::read_to_string(path) {
                Ok(text) => Ok(Some(text)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error.into()),
            }
        };
        match Workspace::load_with(&workspace.root, &read) {
            Ok(base) => Self {
                files: texts.into_keys().collect(),
                base: Some(Box::new(base)),
            },
            // The manifests count as changed whole, as before.
            Err(_) => Self::default(),
        }
    }
}

/// How a project's configuration differs between two workspaces: its
/// targets and named inputs by name, its other keys, and what its
/// dependencies and inputs naming projects select.
#[derive(Default)]
pub(crate) struct Difference {
    pub(crate) targets: Vec<String>,
    pub(crate) named_inputs: Vec<String>,
    pub(crate) fields: Vec<String>,
}

impl Difference {
    pub(crate) fn is_empty(&self) -> bool {
        self.targets.is_empty() && self.named_inputs.is_empty() && self.fields.is_empty()
    }
}

/// How each project of `head` differs from `base`, both the same files with
/// different configuration, leaving out the projects that do not.
pub(crate) fn differences(head: &Workspace, base: &Workspace) -> BTreeMap<String, Difference> {
    let graph = |workspace: &Workspace| {
        qk_graph::ProjectGraph::build(workspace)
            .map(|graph| graph.dependencies)
            .unwrap_or_default()
    };
    let (after, before) = (graph(head), graph(base));
    let edges = |graph: &BTreeMap<String, Vec<qk_graph::Dependency>>, name: &str| {
        graph
            .get(name)
            .into_iter()
            .flatten()
            .map(|edge| (edge.target.clone(), edge.kind.clone()))
            .collect::<BTreeSet<_>>()
    };
    let mut found = BTreeMap::new();
    for project in head.projects.values() {
        let old = base.projects.get(&project.name);
        let mut difference = Difference {
            targets: crate::changed_names(&project.targets, old.map(|old| &old.targets)),
            named_inputs: crate::changed_names(
                &project.named_inputs,
                old.map(|old| &old.named_inputs),
            ),
            fields: Vec::new(),
        };
        let fields = |project: Option<&Project>| match project.map(serde_json::to_value) {
            Some(Ok(Value::Object(mut fields))) => {
                fields.remove("targets");
                fields.remove("namedInputs");
                fields
            }
            _ => serde_json::Map::new(),
        };
        let (new, previous) = (fields(Some(project)), fields(old));
        difference.fields = new
            .keys()
            .chain(previous.keys())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter(|key| new.get(*key) != previous.get(*key))
            .cloned()
            .collect();
        if edges(&after, &project.name) != edges(&before, &project.name) {
            difference.fields.push("dependencies".into());
        }
        if selected(project, head) != selected(project, base) {
            difference.fields.push("projects its inputs name".into());
        }
        if !difference.is_empty() {
            found.insert(project.name.clone(), difference);
        }
    }
    found
}

/// The projects each input of `project` naming projects selects in
/// `workspace`, as `tag:` patterns and names read there.
fn selected(project: &Project, workspace: &Workspace) -> Vec<Vec<String>> {
    let mut patterns = Vec::new();
    let inputs = project
        .targets
        .values()
        .filter_map(|target| target.inputs.as_ref())
        .chain(project.named_inputs.values());
    for input in inputs.flatten() {
        let Some(listed) = input.get("projects") else {
            continue;
        };
        let listed: Vec<String> = match listed {
            Value::String(pattern) => vec![pattern.clone()],
            other => serde_json::from_value(other.clone()).unwrap_or_default(),
        };
        patterns.push(listed);
    }
    patterns
        .iter()
        .map(|listed| {
            qk_config::find_matching_projects(
                listed,
                &workspace.projects,
                qk_config::Reader::Native,
            )
        })
        .collect()
}
