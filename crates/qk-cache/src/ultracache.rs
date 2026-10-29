//! `qk:ultracache`: key a task by what it read when it last ran, rather
//! than by its declared file inputs.
//!
//! A run with `--record` traces each opted-in task it executes and keeps a
//! path set: the files the task read, the directories it listed and the
//! paths it looked up. A later run keys the task by how those paths are now,
//! so a change to a file the task never read is still a hit, and a new file
//! in a directory it listed is a miss. See [`crate::recorded`].
//!
//! - `mode`: `"on"`, the default once the field is set, `"record"` or
//!   `"off"`. With `"record"`, a task is recorded and its recordings kept and
//!   shared as with `"on"`, but it is keyed by its declared inputs, and the
//!   run reports what its recordings would have done. Nx's `"warn"` and
//!   `"error"` are not supported yet.
//! - `ignoreReads`: workspace globs left out of the path set, for files whose
//!   changes cannot change the result. One starting `^` names files in the
//!   projects the task's project depends on, directly or not: see
//!   [`IgnoredReads`].

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result, bail};
use qk_config::Workspace;
use qk_graph::ProjectGraph;
use qk_taskgraph::Task;
use serde_json::Value;

use crate::glob::Pattern;

/// The fields a `qk:ultracache` object can set. Others are ignored, as Nx
/// ignores keys it does not know; `qk doctor` reports them.
pub const FIELDS: &[&str] = &["mode", "ignoreReads"];

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    pub mode: Mode,
    pub ignore_reads: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// Keyed by its recordings once it has any.
    #[default]
    On,
    /// Recorded, but keyed by its declared inputs.
    Record,
}

impl Settings {
    /// The target's settings when it is recorded, `None` when it is not:
    /// without the field, or with `mode` `"off"`.
    pub fn of(task: &Task) -> Result<Option<Self>> {
        let Some(value) = task.definition.extra.get("qk:ultracache") else {
            return Ok(None);
        };
        let Value::Object(object) = value else {
            bail!("qk:ultracache must be an object");
        };
        let mode = match object.get("mode").map(Value::as_str) {
            None | Some(Some("on")) => Mode::On,
            Some(Some("record")) => Mode::Record,
            Some(Some("off")) => return Ok(None),
            Some(Some(mode @ ("warn" | "error"))) => {
                bail!(
                    "qk:ultracache.mode {mode:?} is not supported yet; use \"on\", \"record\" or \"off\""
                )
            }
            Some(_) => bail!("qk:ultracache.mode must be \"on\", \"record\" or \"off\""),
        };
        let ignore_reads = ignored_reads(task)?;
        Ok(Some(Self { mode, ignore_reads }))
    }
}

/// The target's `qk:ultracache.ignoreReads`, whatever its mode: the reads
/// that change nothing the task does, which the sandbox leaves out of its
/// findings and allows whether or not the task is recorded.
pub fn ignored_reads(task: &Task) -> Result<Vec<String>> {
    let Some(Value::Object(object)) = task.definition.extra.get("qk:ultracache") else {
        return Ok(Vec::new());
    };
    let ignore_reads: Vec<String> = match object.get("ignoreReads") {
        None => Vec::new(),
        Some(value) => serde_json::from_value(value.clone())
            .context("qk:ultracache.ignoreReads must be an array of globs")?,
    };
    // Checked before the task runs, rather than after it was traced.
    for glob in &ignore_reads {
        Pattern::new(glob.strip_prefix('^').unwrap_or(glob), false)
            .with_context(|| format!("qk:ultracache.ignoreReads has an invalid glob {glob:?}"))?;
    }
    Ok(ignore_reads)
}

/// What a task's `ignoreReads` leaves out, matched against workspace-relative
/// paths written with `/`.
///
/// An entry is a workspace glob, or, starting `^`, a glob within the projects
/// the task's project depends on, directly or not: it matches a path whose
/// innermost project is one of them, the glob matched against the path from
/// that project's root. A file of the task's own project, or of a project
/// nested in a dependency that it does not depend on, is never matched so.
/// What a tool reads following imports into a dependency, as an import cycle
/// rule does, is named that way without naming the task's own files.
#[derive(Clone, Default)]
pub struct IgnoredReads {
    plain: Vec<(String, Pattern)>,
    dependency: Vec<(String, Pattern)>,
    /// Matched as plain entries, but not walked: the task's scratch
    /// directories, which its recordings leave out too.
    extra: Vec<Pattern>,
    /// Every project's root, the deepest first, and whether the task's
    /// project depends on it; only kept with a `^` entry.
    owners: Vec<(String, bool)>,
}

impl IgnoredReads {
    /// `written` as [`ignored_reads`] returns it, for a task of `project`.
    /// Without `projects`, `^` entries match nothing.
    pub fn new(
        written: &[String],
        workspace: &Workspace,
        projects: Option<&ProjectGraph>,
        project: &str,
    ) -> Result<Self> {
        let mut ignored = Self::of_globs(written)?;
        if ignored.dependency.is_empty() {
            return Ok(ignored);
        }
        let depended = projects
            .map(|projects| depended(projects, project))
            .unwrap_or_default();
        ignored.owners = workspace
            .projects
            .iter()
            .map(|(name, config)| (root_of(&config.root), depended.contains(name.as_str())))
            .collect();
        // Deepest first; of two projects written with the same root, as `a`
        // and `./a`, the one not depended on, so neither is ignored, as the
        // sandbox's report counts neither as a dependency's.
        ignored.owners.sort_by(|a, b| {
            b.0.len()
                .cmp(&a.0.len())
                .then_with(|| a.0.cmp(&b.0))
                .then_with(|| a.1.cmp(&b.1))
        });
        Ok(ignored)
    }

    /// Nothing left out.
    pub const fn none() -> Self {
        Self {
            plain: Vec::new(),
            dependency: Vec::new(),
            extra: Vec::new(),
            owners: Vec::new(),
        }
    }

    /// Workspace globs alone, as a task outside any project graph has them:
    /// `^` entries match nothing.
    pub fn of_globs(written: &[String]) -> Result<Self> {
        let mut ignored = Self::default();
        for glob in written {
            match glob.strip_prefix('^') {
                Some(within) => ignored
                    .dependency
                    .push((within.to_owned(), Pattern::new(within, false)?)),
                None => ignored
                    .plain
                    .push((glob.clone(), Pattern::new(glob, false)?)),
            }
        }
        Ok(ignored)
    }

    /// These, and `extra` matched as plain entries are.
    pub fn with_patterns(mut self, extra: impl IntoIterator<Item = Pattern>) -> Self {
        self.extra.extend(extra);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.plain.is_empty() && self.dependency.is_empty() && self.extra.is_empty()
    }

    pub fn is_match(&self, path: impl AsRef<Path>) -> bool {
        let path = path.as_ref();
        if self.plain.iter().any(|(_, glob)| glob.is_match(path))
            || self.extra.iter().any(|glob| glob.is_match(path))
        {
            return true;
        }
        if self.dependency.is_empty() {
            return false;
        }
        let Some(text) = path.to_str() else {
            return false;
        };
        match self.owner(text) {
            Some((root, true)) => {
                let within = within(root, text);
                self.dependency
                    .iter()
                    .any(|(_, glob)| glob.is_match(within))
            }
            _ => false,
        }
    }

    /// The globs a walk for what this matches starts from, as workspace
    /// globs: the plain entries, and each `^` entry under the root of each
    /// project it can match in.
    pub fn walks(&self) -> (Vec<String>, Vec<String>) {
        let plain = self.plain.iter().map(|(glob, _)| glob.clone()).collect();
        let mut dependency = Vec::new();
        for (root, depended) in &self.owners {
            if !depended {
                continue;
            }
            for (glob, _) in &self.dependency {
                dependency.push(if root.is_empty() {
                    glob.clone()
                } else {
                    format!("{}/{glob}", crate::glob::escape(root))
                });
            }
        }
        (plain, dependency)
    }

    /// Whether a project has its root strictly inside `directory`: a `^`
    /// entry then does not match all that `directory` holds, since it
    /// matches nothing in a project not depended on and matches from its own
    /// root in one that is.
    pub fn project_within(&self, directory: &str) -> bool {
        self.owners.iter().any(|(root, _)| {
            root.len() > directory.len()
                && (directory.is_empty()
                    || root
                        .strip_prefix(directory)
                        .is_some_and(|rest| rest.starts_with('/')))
        })
    }

    /// The roots of the projects `^` entries match in, and of those nested
    /// in them, or written with the same root, that they do not, each with
    /// whether it is depended on, for keys that change with what the entries
    /// match.
    pub fn scope(&self) -> Vec<(&str, bool)> {
        self.owners
            .iter()
            .filter(|(root, depended)| {
                *depended
                    || self.owners.iter().any(|(outer, outer_depended)| {
                        *outer_depended
                            && (root == outer
                                || outer.is_empty()
                                || root
                                    .strip_prefix(outer.as_str())
                                    .is_some_and(|rest| rest.starts_with('/')))
                    })
            })
            .map(|(root, depended)| (root.as_str(), *depended))
            .collect()
    }

    /// The innermost project holding `path`, with whether it is depended on.
    fn owner(&self, path: &str) -> Option<(&str, bool)> {
        self.owners
            .iter()
            .find(|(root, _)| root.is_empty() || within(root, path) != path)
            .map(|(root, depended)| (root.as_str(), *depended))
    }
}

/// `path` from `root`, or `path` itself when `root` does not hold it.
fn within<'a>(root: &str, path: &'a str) -> &'a str {
    if root.is_empty() {
        return path;
    }
    path.strip_prefix(root)
        .and_then(|rest| rest.strip_prefix('/'))
        .unwrap_or(path)
}

/// A project root as paths are named: `./libs/lib/` is `libs/lib`, and the
/// workspace's root project is empty.
fn root_of(root: &str) -> String {
    root.split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect::<Vec<_>>()
        .join("/")
}

/// The projects `project` depends on, directly or not, itself left out.
fn depended(projects: &ProjectGraph, project: &str) -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    let mut pending = vec![project];
    while let Some(next) = pending.pop() {
        for dependency in projects.dependencies.get(next).into_iter().flatten() {
            if dependency.target != project && seen.insert(dependency.target.clone()) {
                pending.push(&dependency.target);
            }
        }
    }
    seen
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(value: Value) -> Task {
        let mut definition: qk_config::Target =
            serde_json::from_value(serde_json::json!({})).unwrap();
        definition.extra.insert("qk:ultracache".into(), value);
        Task {
            id: "app:build".into(),
            project: "app".into(),
            target: "build".into(),
            configuration: None,
            args: Vec::new(),
            definition,
            dependencies: Default::default(),
            edges: Default::default(),
        }
    }

    /// `app` at `apps/app` depends on `lib`, which holds `nested`, which it
    /// does not, and on `outer`, whose root holds `app` itself.
    fn workspace() -> (tempfile::TempDir, Workspace, ProjectGraph) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::write(root.join("nx.json"), "{}").unwrap();
        for (directory, name, depends) in [
            ("apps/app", "app", vec!["lib", "outer", "inner"]),
            ("apps", "outer", vec![]),
            ("packages/lib", "lib", vec![]),
            ("packages/lib/nested", "nested", vec![]),
            ("packages/lib/src/inner", "inner", vec![]),
            ("packages/other", "other", vec![]),
        ] {
            std::fs::create_dir_all(root.join(directory)).unwrap();
            std::fs::write(
                root.join(directory).join("project.json"),
                serde_json::json!({"name": name, "implicitDependencies": depends}).to_string(),
            )
            .unwrap();
        }
        let workspace = Workspace::load(root).unwrap();
        let projects = ProjectGraph::build(&workspace).unwrap();
        (temp, workspace, projects)
    }

    #[test]
    fn a_dependency_entry_matches_only_the_projects_depended_on() {
        let (_temp, workspace, projects) = workspace();
        let written = ["^**/*.ts".to_owned(), "tmp/**".to_owned()];
        let ignored = IgnoredReads::new(&written, &workspace, Some(&projects), "app").unwrap();
        for (path, matched) in [
            ("packages/lib/src/a.ts", true),
            ("packages/lib/src/a.json", false),
            // A project nested in a dependency that is not one itself
            ("packages/lib/nested/src/a.ts", false),
            ("packages/other/a.ts", false),
            // Its own files, though a dependency's root holds them
            ("apps/app/src/a.ts", false),
            ("apps/z.ts", true),
            ("tmp/x", true),
        ] {
            assert_eq!(ignored.is_match(path), matched, "{path}");
        }
        let (plain, dependency) = ignored.walks();
        assert_eq!(plain, ["tmp/**"]);
        assert_eq!(
            dependency,
            [
                "packages/lib/src/inner/**/*.ts",
                "packages/lib/**/*.ts",
                "apps/**/*.ts"
            ]
        );
        assert!(ignored.project_within("packages/lib"));
        // A dependency nested in one, whose files `^` entries match from its
        // own root
        assert!(ignored.project_within("packages/lib/src"));
        assert!(!ignored.project_within("packages/lib/src/inner"));
        assert!(!ignored.project_within("packages/lib/nested/src"));
        assert!(ignored.project_within("apps"));
        assert_eq!(
            ignored.scope(),
            [
                ("packages/lib/src/inner", true),
                ("packages/lib/nested", false),
                ("packages/lib", true),
                ("apps/app", false),
                ("apps", true)
            ]
        );
        // Without the graph, or without a `^` entry, nothing depends on it.
        let alone = IgnoredReads::new(&written, &workspace, None, "app").unwrap();
        assert!(!alone.is_match("packages/lib/src/a.ts"));
        assert!(alone.is_match("tmp/x"));
    }

    #[test]
    fn reads_the_mode_and_ignored_reads() {
        let on = Settings::of(&task(serde_json::json!({"ignoreReads": ["tmp/**"]})))
            .unwrap()
            .unwrap();
        assert_eq!(on.ignore_reads, ["tmp/**"]);
        assert_eq!(on.mode, Mode::On);
        let record = Settings::of(&task(serde_json::json!({"mode": "record"})))
            .unwrap()
            .unwrap();
        assert_eq!(record.mode, Mode::Record);
        assert!(
            Settings::of(&task(serde_json::json!({"mode": "off"})))
                .unwrap()
                .is_none()
        );
        // Unrecorded, its ignored reads still hold for the sandbox.
        let off = task(serde_json::json!({"mode": "off", "ignoreReads": ["tmp/**"]}));
        assert_eq!(ignored_reads(&off).unwrap(), ["tmp/**"]);
        for invalid in [
            serde_json::json!(true),
            serde_json::json!({"mode": "warn"}),
            serde_json::json!({"mode": 1}),
            serde_json::json!({"ignoreReads": "tmp/**"}),
            serde_json::json!({"ignoreReads": ["src/[*.log"]}),
        ] {
            assert!(Settings::of(&task(invalid.clone())).is_err(), "{invalid}");
        }
    }
}
