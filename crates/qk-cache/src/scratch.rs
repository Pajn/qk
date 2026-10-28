//! `qk:sandbox`: `false` runs a task unsandboxed under `--sandbox`. An object
//! sandboxes it, and its `scratch` names directories in the workspace the task
//! keeps nothing in between runs, such as a tool's cache it cannot be told to
//! keep elsewhere.
//!
//! qk empties each scratch directory before the task runs, with or without
//! `--sandbox`, so nothing an earlier run left there can change a result,
//! whether or not the tool checks what it finds. The cache neither keeps nor
//! restores them, a recording leaves them out, and the sandbox lets the task
//! read and write them. A tool whose cache is safe to keep, as one checking
//! every entry against what it read, is better kept as warm state.

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use qk_config::Workspace;
use qk_taskgraph::{Task, TaskGraph};
use serde_json::Value;

/// The fields a `qk:sandbox` object can set. Others are ignored, as Nx
/// ignores keys it does not know; `qk doctor` reports them.
pub const FIELDS: &[&str] = &["scratch"];

/// What a target's `qk:sandbox` says.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    /// Whether the task runs sandboxed under `--sandbox`.
    pub sandboxed: bool,
    /// Its scratch directories, as written.
    pub scratch: Vec<String>,
}

impl Settings {
    /// The settings of a target's `qk:sandbox` value: sandboxed without it.
    pub fn of(value: Option<&Value>) -> Result<Self> {
        let scratch = match value {
            None | Some(Value::Bool(true)) => Vec::new(),
            Some(Value::Bool(false)) => {
                return Ok(Self {
                    sandboxed: false,
                    scratch: Vec::new(),
                });
            }
            Some(Value::Object(object)) => match object.get("scratch") {
                None => Vec::new(),
                Some(value) => serde_json::from_value(value.clone())
                    .context("qk:sandbox.scratch must be an array of paths")?,
            },
            Some(_) => bail!("qk:sandbox must be true, false or an object"),
        };
        Ok(Self {
            sandboxed: true,
            scratch,
        })
    }
}

/// A task's scratch directories, absolute: each must lie inside the
/// workspace, outside `.git`, `.qk` and `node_modules` in any case, hold no
/// project, nor the cache or qk's state, and neither be nor lie under a
/// symbolic link, which emptying it could follow out of the workspace.
pub fn directories(workspace: &Workspace, task: &Task) -> Result<Vec<PathBuf>> {
    directories_of(
        workspace,
        &task.project,
        task.definition.extra.get("qk:sandbox"),
    )
}

/// [`directories`] for a `qk:sandbox` value of one of `project`'s targets.
pub fn directories_of(
    workspace: &Workspace,
    project: &str,
    value: Option<&Value>,
) -> Result<Vec<PathBuf>> {
    let settings = Settings::of(value)?;
    if settings.scratch.is_empty() {
        return Ok(Vec::new());
    }
    let projects: Vec<&str> = workspace
        .projects
        .values()
        .map(|project| project.root.as_str())
        .collect();
    // Where qk keeps its own, which the workspace holds outside Git, as the
    // file system resolves it.
    let kept = [
        resolved(&crate::paths::worktree_state(&workspace.root)),
        resolved(&crate::paths::cache_location(workspace)),
    ];
    settings
        .scratch
        .iter()
        .map(|written| {
            let expanded = crate::paths::expand(workspace, project, written)?;
            let directory = check(&expanded, &projects)
                .and_then(|relative| {
                    unlinked(&workspace.root, &relative)?;
                    let directory = workspace.root.join(&relative);
                    if kept.iter().any(|kept| nested(kept, &resolved(&directory))) {
                        bail!("it would empty qk's cache or state");
                    }
                    Ok(directory)
                })
                .with_context(|| {
                    format!("qk:sandbox.scratch {written:?} cannot be a scratch directory")
                })?;
            Ok(directory)
        })
        .collect()
}

/// Two tasks in a run cannot share a scratch directory, nor have one inside
/// another's: emptying it before one runs would take what the other is
/// using.
pub fn check_overlaps(workspace: &Workspace, graph: &TaskGraph) -> Result<()> {
    let mut claimed: Vec<(PathBuf, &str)> = Vec::new();
    for (id, task) in &graph.tasks {
        let named = directories(workspace, task).with_context(|| format!("cannot execute {id}"))?;
        for directory in named {
            if let Some((_, other)) = claimed
                .iter()
                .find(|(known, other)| *other != id && nested(known, &directory))
            {
                let shown = directory
                    .strip_prefix(&workspace.root)
                    .unwrap_or(&directory);
                bail!(
                    "{id} and {other} both use {} as scratch, which each empties before it runs",
                    shown.display()
                );
            }
            claimed.push((directory, id));
        }
    }
    Ok(())
}

/// `path` as the file system resolves it: the part of it that exists with
/// its links and `..` followed, then the rest with `..` taken away.
fn resolved(path: &Path) -> PathBuf {
    let mut existing = path;
    let mut rest = Vec::new();
    loop {
        if let Ok(mut real) = existing.canonicalize() {
            for part in rest.into_iter().rev() {
                match part {
                    Component::ParentDir => {
                        real.pop();
                    }
                    Component::CurDir => {}
                    part => real.push(part),
                }
            }
            return real;
        }
        match (existing.parent(), existing.components().next_back()) {
            (Some(parent), Some(last)) => {
                rest.push(last);
                existing = parent;
            }
            _ => return path.to_owned(),
        }
    }
}

/// Whether one of two paths is, or lies under, the other, comparing names
/// without regard to case, as a file system may hold them.
fn nested(a: &Path, b: &Path) -> bool {
    let names = |path: &Path| -> Vec<String> {
        path.components()
            .map(|part| part.as_os_str().to_string_lossy().to_lowercase())
            .collect()
    };
    let (a, b) = (names(a), names(b));
    a.starts_with(&b) || b.starts_with(&a)
}

/// Why `relative`, below `root`, could lead out of it: a part of it that
/// exists as a symbolic link.
fn unlinked(root: &Path, relative: &Path) -> Result<()> {
    let mut path = root.to_owned();
    for part in Path::new(relative).components() {
        path.push(part);
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!("it is or lies under a symbolic link, which emptying it could follow")
            }
            Ok(_) => {}
            // Nothing below a part that does not exist can be a link yet.
            Err(_) => return Ok(()),
        }
    }
    Ok(())
}

/// `written`, a workspace path, as its parts, or why it cannot be emptied
/// before each run. Names are compared without regard to case, as a
/// file system may hold them.
fn check(written: &str, projects: &[&str]) -> Result<PathBuf> {
    if written.contains(['*', '?', '[', '{']) {
        bail!("it is a glob, where a directory is named");
    }
    let named = parts(written)?;
    if named.is_empty() {
        bail!("it would empty the workspace");
    }
    if named
        .iter()
        .any(|part| matches!(part.as_str(), ".git" | ".qk" | "node_modules"))
    {
        bail!("it lies in .git, .qk or node_modules");
    }
    for project in projects {
        // A project the workspace holds at its root holds every path.
        if let Ok(root) = parts(project)
            && !root.is_empty()
            && root.starts_with(&named)
        {
            bail!("it would empty the project in {project}");
        }
    }
    Ok(written
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect())
}

/// The parts of `path`, a workspace path, in lower case, or why it is none.
fn parts(path: &str) -> Result<Vec<String>> {
    Path::new(path)
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(Ok(part.to_string_lossy().to_lowercase())),
            Component::CurDir => None,
            _ => Some(Err(anyhow::anyhow!(
                "it is not a path inside the workspace"
            ))),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_false_true_and_an_object() {
        assert!(!Settings::of(Some(&Value::Bool(false))).unwrap().sandboxed);
        assert!(Settings::of(None).unwrap().sandboxed);
        let settings = Settings::of(Some(
            &serde_json::json!({"scratch": ["{projectRoot}/.cache"]}),
        ))
        .unwrap();
        assert!(settings.sandboxed);
        assert_eq!(settings.scratch, ["{projectRoot}/.cache"]);
        assert!(Settings::of(Some(&serde_json::json!("no"))).is_err());
        assert!(Settings::of(Some(&serde_json::json!({"scratch": "x"}))).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn refuses_a_path_through_a_symbolic_link() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("link")).unwrap();
        std::fs::create_dir(root.path().join("real")).unwrap();
        assert!(unlinked(root.path(), Path::new("link/cache")).is_err());
        assert!(unlinked(root.path(), Path::new("link")).is_err());
        assert!(unlinked(root.path(), Path::new("real/cache")).is_ok());
        assert!(unlinked(root.path(), Path::new("missing/cache")).is_ok());
    }

    #[test]
    fn resolves_what_exists_and_takes_away_the_rest() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().canonicalize().unwrap();
        std::fs::create_dir(root.path().join("foo")).unwrap();
        assert_eq!(
            resolved(&root.path().join("foo/../scratch")),
            real.join("scratch")
        );
        assert_eq!(
            resolved(&root.path().join("missing/../scratch/cache")),
            real.join("scratch/cache")
        );
    }

    #[test]
    fn nests_paths_whatever_their_case() {
        assert!(nested(Path::new("/w/Cache"), Path::new("/w/cache/lint")));
        assert!(nested(
            Path::new("/w/build/cache"),
            Path::new("/w/Build/Cache")
        ));
        assert!(!nested(Path::new("/w/cache"), Path::new("/w/cached")));
    }

    #[test]
    fn refuses_what_cannot_be_emptied() {
        let projects = ["./apps/app/", "libs/lib", "."];
        assert_eq!(
            check("./apps/app/.fallow/", &projects).unwrap(),
            Path::new("apps/app/.fallow")
        );
        for refused in [
            "apps/app",
            "apps/app/",
            "./apps/app",
            "Apps/App",
            "apps",
            "libs",
            "",
            ".",
            "../outside",
            "apps/../../outside",
            "apps/app/node_modules/.cache",
            "apps/app/NODE_MODULES/.cache",
            ".git/qk",
            ".GIT/qk",
            ".qk/cache",
            "apps/app/**/cache",
        ] {
            assert!(check(refused, &projects).is_err(), "{refused}");
        }
    }
}
