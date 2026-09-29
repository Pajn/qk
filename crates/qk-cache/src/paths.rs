use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context, Result, bail};
use qk_config::Workspace;
use qk_taskgraph::Task;

pub fn git(root: &Path, args: &[&str]) -> std::io::Result<Output> {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
        .output()
}

pub fn git_path(root: &Path, flag: &str) -> Option<PathBuf> {
    let result = git(root, &["rev-parse", "--path-format=absolute", flag]).ok()?;
    if !result.status.success() {
        return None;
    }
    let path = String::from_utf8(result.stdout).ok()?;
    Some(PathBuf::from(path.trim_end_matches(['\r', '\n'])))
}

/// Where qk keeps what belongs to one worktree: digests of its files, records
/// of the outputs it holds, and restores in progress. Inside Git that is the
/// worktree's own git directory, so nothing appears in the working tree;
/// outside Git it is `.qk` at the workspace root.
pub fn worktree_state(root: &Path) -> PathBuf {
    static KNOWN: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<PathBuf, PathBuf>>,
    > = std::sync::OnceLock::new();
    let known = KNOWN.get_or_init(Default::default);
    if let Some(state) = known.lock().unwrap().get(root) {
        return state.clone();
    }
    let state = git_path(root, "--git-dir")
        .map(|dir| dir.join("qk"))
        .unwrap_or_else(|| root.join(".qk"));
    known.lock().unwrap().insert(root.to_owned(), state.clone());
    state
}

pub fn cache_directory(root: &Path) -> PathBuf {
    git_path(root, "--git-common-dir")
        .map(|dir| dir.join("qk/cache/v1"))
        .unwrap_or_else(|| root.join(".qk/cache/v1"))
}

pub fn relative(root: &Path, path: &Path) -> Result<String> {
    let parts = path
        .strip_prefix(root)?
        .components()
        .map(|part| {
            part.as_os_str()
                .to_str()
                .map(str::to_owned)
                .context("cache paths must be UTF-8")
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(parts.join("/"))
}

pub fn validate_path(path: &str) -> Result<()> {
    if path.is_empty()
        || path.contains(['\\', ':', '\0'])
        || Path::new(path).is_absolute()
        || Path::new(path)
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
        || path
            .split('/')
            .any(|part| matches!(part, ".git" | ".qk" | ".." | "." | ""))
    {
        bail!("unsafe cache artifact path {path:?}");
    }
    Ok(())
}

pub fn safe_parents(root: &Path, path: &str) -> Result<()> {
    validate_path(path)?;
    let mut current = root.to_owned();
    for part in Path::new(path)
        .parent()
        .into_iter()
        .flat_map(Path::components)
    {
        current.push(part);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!("cache artifact parent is a symlink: {}", current.display())
            }
            Ok(metadata) if !metadata.is_dir() => bail!(
                "cache artifact parent is not a directory: {}",
                current.display()
            ),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

pub use crate::glob::Pattern;

pub fn expand(workspace: &Workspace, project: &str, text: &str) -> Result<String> {
    let project_root = &workspace.projects[project].root;
    let prefix = if project_root == "." {
        String::new()
    } else {
        format!("{project_root}/")
    };
    let result = text
        .replace("{workspaceRoot}/", "")
        .replace("{projectRoot}/", &prefix)
        .replace("{projectRoot}", project_root)
        .replace("{workspaceRoot}", ".");
    if result.contains("{options.") || result.contains("{args.") {
        bail!("dynamic cache paths are not supported yet");
    }
    normalize(&result)
}

/// Resolves `.` and `..` segments lexically, e.g. `app/e2e/../ios` to `app/ios`.
fn normalize(path: &str) -> Result<String> {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "." => {}
            ".." => match parts.pop() {
                Some(parent) if crate::glob::is_literal(parent) => {}
                _ => bail!("cache path {path:?} leaves the workspace or follows a glob with `..`"),
            },
            part => parts.push(part),
        }
    }
    Ok(if parts.is_empty() {
        ".".to_owned()
    } else {
        parts.join("/")
    })
}

#[derive(Clone)]
pub struct Outputs {
    patterns: Vec<Pattern>,
    /// Negated patterns: what they match is never an output.
    negations: Vec<Pattern>,
    anchors: BTreeSet<String>,
    /// Whether the target declares its outputs, rather than taking Nx's
    /// defaults for a target without them.
    explicit: bool,
}

impl Outputs {
    /// Paths from already-expanded workspace-relative patterns, each with a
    /// fixed directory prefix, as outputs require.
    pub fn from_paths(patterns: &[String]) -> Result<Self> {
        let mut compiled = Vec::new();
        let mut anchors = BTreeSet::new();
        for pattern in patterns {
            validate_path(pattern)?;
            let anchor = pattern
                .split('/')
                .take_while(|part| crate::glob::is_literal(part))
                .collect::<Vec<_>>()
                .join("/");
            if anchor.is_empty() {
                bail!("{pattern:?} needs a fixed directory prefix");
            }
            compiled.push(Pattern::new(pattern, false)?);
            anchors.insert(anchor);
        }
        Ok(Self {
            patterns: compiled,
            negations: Vec::new(),
            anchors,
            explicit: true,
        })
    }

    /// Outputs from already-expanded patterns, for tests without a workspace.
    #[cfg(test)]
    pub fn from_patterns(patterns: &[&str]) -> Self {
        Self {
            patterns: patterns
                .iter()
                .map(|pattern| Pattern::new(pattern, false).unwrap())
                .collect(),
            anchors: patterns
                .iter()
                .map(|pattern| (*pattern).to_owned())
                .collect(),
            negations: Vec::new(),
            explicit: true,
        }
    }

    /// The task's outputs as Nx resolves them. `{options.name}` reads the
    /// target's options with the task's arguments applied, and an output
    /// naming what cannot be resolved is left out. Without `outputs`, Nx takes
    /// `options.outputPath`, and for `build` and `prepare` targets
    /// `dist/<root>`, `<root>/dist`, `<root>/build` and `<root>/public`.
    pub fn new(workspace: &Workspace, task: &Task) -> Result<Self> {
        let root = &workspace.projects[&task.project].root;
        let options = task_options(task);
        let (templates, explicit) = match &task.definition.outputs {
            Some(outputs) => (outputs.clone(), true),
            None => match options.get("outputPath") {
                Some(serde_json::Value::String(path)) => (vec![path.clone()], false),
                Some(serde_json::Value::Array(paths)) => (
                    paths
                        .iter()
                        .filter_map(|path| path.as_str().map(str::to_owned))
                        .collect(),
                    false,
                ),
                _ if matches!(task.target.as_str(), "build" | "prepare") => (
                    [
                        format!("dist/{root}"),
                        format!("{root}/dist"),
                        format!("{root}/build"),
                        format!("{root}/public"),
                    ]
                    .into(),
                    false,
                ),
                _ => (Vec::new(), false),
            },
        };
        let mut patterns = Vec::new();
        let mut negations = Vec::new();
        let mut anchors = BTreeSet::new();
        for template in templates {
            let (negated, template) = match template.strip_prefix('!') {
                Some(rest) => (true, rest.to_owned()),
                None => (false, template),
            };
            let Some(resolved) = resolve_output(task, &options, &template) else {
                continue;
            };
            let pattern = expand(workspace, &task.project, &resolved)?
                .trim_end_matches('/')
                .to_owned();
            validate_path(&pattern)?;
            if negated {
                negations.push(Pattern::new(&pattern, false)?);
                continue;
            }
            let anchor = pattern
                .split('/')
                .take_while(|part| crate::glob::is_literal(part))
                .collect::<Vec<_>>()
                .join("/");
            if anchor.is_empty()
                || workspace
                    .projects
                    .values()
                    .any(|project| project.root == pattern)
            {
                bail!(
                    "cache outputs must have a fixed directory prefix and cannot replace a project root"
                );
            }
            patterns.push(Pattern::new(&pattern, false)?);
            anchors.insert(anchor);
        }
        Ok(Self {
            patterns,
            negations,
            anchors,
            explicit,
        })
    }

    /// Whether the outputs come from the target's `outputs`.
    pub fn is_explicit(&self) -> bool {
        self.explicit
    }

    /// Whether the task declares outputs that hold everything it produces,
    /// so dependents may be keyed by their content alone. Nx's defaults for a
    /// target without `outputs` are only guesses, and do not count.
    pub fn declared(&self) -> bool {
        self.explicit && !self.patterns.is_empty()
    }

    /// Fixed directory prefixes; every matching path lies below one of them.
    pub fn anchors(&self) -> impl Iterator<Item = &str> {
        self.anchors.iter().map(String::as_str)
    }

    pub fn matches(&self, path: &str) -> bool {
        let any = |patterns: &[Pattern]| {
            Path::new(path)
                .ancestors()
                .any(|ancestor| patterns.iter().any(|pattern| pattern.is_match(ancestor)))
        };
        any(&self.patterns) && !any(&self.negations)
    }

    pub fn paths(&self, root: &Path) -> Result<BTreeSet<String>> {
        let mut paths = BTreeSet::new();
        for anchor in &self.anchors {
            safe_parents(root, anchor)?;
            let path = root.join(anchor);
            match std::fs::symlink_metadata(&path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
                Ok(_) => {}
            }
            for entry in walkdir::WalkDir::new(path).follow_links(false) {
                let entry = entry?;
                let path = relative(root, entry.path())?;
                if self.matches(&path) {
                    validate_path(&path)?;
                    paths.insert(path);
                }
            }
        }
        Ok(paths)
    }
}

/// The target's options with the task's `--name=value` arguments applied,
/// as Nx's overrides are.
fn task_options(task: &Task) -> serde_json::Map<String, serde_json::Value> {
    let mut options = task.definition.options.clone();
    let mut args = task.args.iter().peekable();
    while let Some(arg) = args.next() {
        let Some(flag) = arg.strip_prefix("--") else {
            continue;
        };
        let (key, value) = match flag.split_once('=') {
            Some((key, value)) => (key.to_owned(), value.to_owned()),
            None if args.peek().is_some_and(|next| !next.starts_with('-')) => {
                (flag.to_owned(), args.next().unwrap().clone())
            }
            None => (flag.to_owned(), "true".to_owned()),
        };
        options.insert(key, serde_json::Value::String(value));
    }
    options
}

/// An output with `{options.*}`, `{projectName}` and the legacy
/// `{project.name}` and `{project.root}` replaced, or `None` when one of
/// them has no value, which Nx leaves the output out for.
fn resolve_output(
    task: &Task,
    options: &serde_json::Map<String, serde_json::Value>,
    template: &str,
) -> Option<String> {
    let mut result = String::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        result.push_str(&rest[..start]);
        let end = rest[start..].find('}')? + start;
        let token = rest[start + 1..end].trim();
        let value = if let Some(path) = token.strip_prefix("options.") {
            let mut value = options.get(path.split('.').next()?)?;
            for part in path.split('.').skip(1) {
                value = value.get(part)?;
            }
            match value {
                serde_json::Value::String(text) if !text.is_empty() => text.clone(),
                serde_json::Value::Number(number) => number.to_string(),
                serde_json::Value::Bool(true) => "true".into(),
                _ => return None,
            }
        } else {
            match token {
                "projectName" | "project.name" => task.project.clone(),
                "project.root" => "{projectRoot}".into(),
                "projectRoot" | "workspaceRoot" => format!("{{{token}}}"),
                _ => return None,
            }
        };
        result.push_str(&value);
        rest = &rest[end + 1..];
    }
    result.push_str(rest);
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::normalize;

    #[test]
    fn normalizes_relative_segments() {
        assert_eq!(
            normalize("apps/mobile/test-e2e/../ios/build/*/App.app").unwrap(),
            "apps/mobile/ios/build/*/App.app"
        );
        assert_eq!(normalize("./dist/./a").unwrap(), "dist/a");
        assert_eq!(normalize(".").unwrap(), ".");
        assert!(normalize("app/../../outside").is_err());
        assert!(normalize("app/**/../x").is_err());
    }
}
