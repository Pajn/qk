use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context, Result, bail};
use globset::{GlobBuilder, GlobMatcher};
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

pub fn matcher(pattern: &str) -> Result<GlobMatcher> {
    if ["!(", "?(", "+(", "*(", "@("]
        .iter()
        .any(|token| pattern.contains(token))
    {
        bail!("extended glob patterns are not supported by the cache yet");
    }
    Ok(GlobBuilder::new(pattern)
        .literal_separator(true)
        .backslash_escape(false)
        .build()?
        .compile_matcher())
}

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
    Ok(result)
}

#[derive(Clone)]
pub struct Outputs {
    patterns: Vec<GlobMatcher>,
    anchors: BTreeSet<String>,
}

impl Outputs {
    /// Outputs from already-expanded patterns, for tests without a workspace.
    #[cfg(test)]
    pub fn from_patterns(patterns: &[&str]) -> Self {
        Self {
            patterns: patterns
                .iter()
                .map(|pattern| matcher(pattern).unwrap())
                .collect(),
            anchors: patterns
                .iter()
                .map(|pattern| (*pattern).to_owned())
                .collect(),
        }
    }

    pub fn new(workspace: &Workspace, task: &Task) -> Result<Self> {
        let mut patterns = Vec::new();
        let mut anchors = BTreeSet::new();
        for pattern in task.definition.outputs.as_deref().unwrap_or_default() {
            let pattern = expand(workspace, &task.project, pattern)?
                .trim_end_matches('/')
                .to_owned();
            if pattern.starts_with('!') {
                bail!("negated outputs are not supported by the cache yet");
            }
            validate_path(&pattern)?;
            let anchor = pattern
                .split('/')
                .take_while(|part| !part.contains(['*', '?', '[', '{']))
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
            patterns.push(matcher(&pattern)?);
            anchors.insert(anchor);
        }
        Ok(Self { patterns, anchors })
    }

    pub fn matches(&self, path: &str) -> bool {
        Path::new(path).ancestors().any(|ancestor| {
            self.patterns
                .iter()
                .any(|pattern| pattern.is_match(ancestor))
        })
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
