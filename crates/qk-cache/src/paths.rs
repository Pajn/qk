use std::collections::{BTreeMap, BTreeSet};
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

/// The worktree's git directory and the repository's common one, read from
/// the `.git` entry at or above `root`, for when `git` cannot answer. A linked
/// worktree's `.git` is a file naming its git directory, whose `commondir`
/// names the common one; without this, a failed `git` would put a linked
/// worktree's state and history at its root as if it were outside Git.
fn dot_git(root: &Path) -> Option<(PathBuf, PathBuf)> {
    let (parent, entry) = root
        .ancestors()
        .map(|dir| (dir, dir.join(".git")))
        .find(|(_, entry)| entry.exists())?;
    let git_dir = if entry.is_dir() {
        entry
    } else {
        let content = std::fs::read_to_string(&entry).ok()?;
        let named = content.strip_prefix("gitdir:")?.trim();
        parent.join(named)
    };
    let common = match std::fs::read_to_string(git_dir.join("commondir")) {
        Ok(named) => git_dir.join(named.trim()),
        Err(_) => git_dir.clone(),
    };
    let absolute = |path: PathBuf| std::fs::canonicalize(&path).unwrap_or(path);
    Some((absolute(git_dir), absolute(common)))
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
        .or_else(|| dot_git(root).map(|(git_dir, _)| git_dir))
        .map(|dir| dir.join("qk"))
        .unwrap_or_else(|| root.join(".qk"));
    known.lock().unwrap().insert(root.to_owned(), state.clone());
    state
}

/// What [`worktree_state`] holds. Inside Git, the state directory of the main
/// worktree also holds the shared cache and run history, so these entries are
/// removed one by one rather than the directory as a whole.
const WORKTREE_ENTRIES: &[&str] = &["digests.json", "outputs", "restore", "sandbox", "warm"];

/// Removes a worktree's state: its file digests, the outputs it records
/// holding, its warm directories and any restores or sandboxes left behind.
pub fn clear_worktree_state(root: &Path) -> Result<()> {
    let state = worktree_state(root);
    for entry in WORKTREE_ENTRIES {
        let path = state.join(entry);
        let removed = if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        match removed {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err(error).with_context(|| format!("cannot remove {}", path.display()));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Where the cache is: as in Nx, `NX_CACHE_DIRECTORY`, then nx.json
/// `cacheDirectory`, relative to the workspace root, with qk's entries in
/// `qk/v1` inside so they never mix with Nx's; otherwise
/// [`cache_directory`], shared by the repository's worktrees.
pub fn cache_location(workspace: &Workspace) -> PathBuf {
    let configured = std::env::var_os("NX_CACHE_DIRECTORY")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            workspace
                .config
                .extra
                .get("cacheDirectory")
                .and_then(serde_json::Value::as_str)
                .map(PathBuf::from)
        });
    match configured {
        Some(directory) => workspace.root.join(directory).join("qk/v1"),
        None => cache_directory(&workspace.root),
    }
}

/// The cache's default place, in the repository's common git directory.
pub fn cache_directory(root: &Path) -> PathBuf {
    git_path(root, "--git-common-dir")
        .or_else(|| dot_git(root).map(|(_, common)| common))
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
        || path.contains(['\\', '\0'])
        || (cfg!(windows) && path.contains(':'))
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

/// [`validate_path`] for an output pattern, whose `\` escapes the character
/// after it, as in Nx: the paths it can match must be safe.
fn validate_pattern(pattern: &str) -> Result<()> {
    let mut path = String::new();
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        path.push(if c == '\\' {
            chars
                .next()
                .with_context(|| format!("unsafe cache artifact path {pattern:?}"))?
        } else {
            c
        });
    }
    validate_path(&path)
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

/// A fileset path with its tokens replaced, as Nx resolves an input's:
/// `{projectRoot}` and `{projectName}` anywhere, and `{workspaceRoot}` only
/// at the start. Anywhere else `{workspaceRoot}` is glob text, as it is in
/// Nx, and so is any other brace group.
pub fn expand(workspace: &Workspace, project: &str, text: &str) -> Result<String> {
    let root = &workspace.projects[project].root;
    expand_with(text, root, project)
}

/// [`expand`] for a glob: the project root and name go in with their glob
/// characters escaped, so a root such as `apps/(marketing)` names that
/// directory rather than a pattern. Nx inserts them as they are.
pub fn expand_glob(workspace: &Workspace, project: &str, text: &str) -> Result<String> {
    let root = &workspace.projects[project].root;
    let root = if root == "." {
        root.clone()
    } else {
        crate::glob::escape(root)
    };
    expand_with(text, &root, &crate::glob::escape(project))
}

fn expand_with(text: &str, project_root: &str, project_name: &str) -> Result<String> {
    let prefix = if project_root == "." {
        String::new()
    } else {
        format!("{project_root}/")
    };
    let text = match text.strip_prefix("{workspaceRoot}") {
        Some(rest) => rest.strip_prefix('/').unwrap_or(match rest {
            "" => ".",
            rest => rest,
        }),
        None => text,
    };
    let result = text
        .replace("{projectRoot}/", &prefix)
        .replace("{projectRoot}", project_root)
        .replace("{projectName}", project_name);
    normalize(&result)
}

/// Resolves `.` and `..` segments lexically, e.g. `app/e2e/../ios` to `app/ios`.
fn normalize(path: &str) -> Result<String> {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "." => {}
            ".." => match parts.pop() {
                Some(parent) if crate::glob::names_a_path(parent) => {}
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

type OutputEntries<'a> = Box<dyn Iterator<Item = Result<(String, std::fs::Metadata)>> + 'a>;

#[derive(Clone)]
pub struct Outputs {
    patterns: Vec<Pattern>,
    /// The patterns as written, for skipping directories none can match.
    globs: Vec<String>,
    /// Negated patterns: what they match is never an output.
    negations: Vec<Pattern>,
    /// The directories the patterns are read from, their leading literal
    /// segments with escapes read, as Nx's `partition_glob` takes them.
    anchors: BTreeSet<String>,
    /// Literal output roots which include their entire subtree, with their
    /// escapes read.
    complete: BTreeSet<String>,
    /// Whether the target declares its outputs, rather than taking Nx's
    /// defaults for a target without them.
    explicit: bool,
}

impl Outputs {
    /// Paths from already-expanded workspace-relative patterns, each with a
    /// fixed directory prefix, as outputs require. A pattern starting with `!`
    /// excludes what it matches.
    pub fn from_paths(patterns: &[String]) -> Result<Self> {
        let mut compiled = Vec::new();
        let mut globs = Vec::new();
        let mut negations = Vec::new();
        let mut anchors = BTreeSet::new();
        let mut complete = BTreeSet::new();
        for pattern in patterns {
            if let Some(excluded) = pattern.strip_prefix('!') {
                validate_pattern(excluded)?;
                negations.push(Pattern::new(excluded, false)?);
                continue;
            }
            validate_pattern(pattern)?;
            let (anchor, glob) = crate::ignored::partition(pattern);
            if anchor.is_empty() {
                bail!("{pattern:?} needs a fixed directory prefix");
            }
            if glob.is_none() {
                complete.insert(anchor.clone());
            }
            compiled.push(Pattern::new(pattern, false)?);
            globs.push(pattern.clone());
            anchors.insert(anchor);
        }
        Ok(Self {
            patterns: compiled,
            globs,
            negations,
            anchors,
            complete,
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
            globs: patterns
                .iter()
                .map(|pattern| (*pattern).to_owned())
                .collect(),
            anchors: patterns
                .iter()
                .map(|pattern| (*pattern).to_owned())
                .collect(),
            complete: patterns
                .iter()
                .filter(|pattern| pattern.split('/').all(crate::glob::is_literal))
                .map(|pattern| (*pattern).to_owned())
                .collect(),
            negations: Vec::new(),
            explicit: true,
        }
    }

    /// The task's outputs, from [`resolved_outputs`].
    pub fn new(workspace: &Workspace, task: &Task) -> Result<Self> {
        let (resolved, explicit) = resolved_outputs(workspace, task)?;
        let mut patterns = Vec::new();
        let mut globs = Vec::new();
        let mut negations = Vec::new();
        let mut anchors = BTreeSet::new();
        let mut complete = BTreeSet::new();
        for output in resolved {
            let (negated, pattern) = match output.strip_prefix('!') {
                Some(rest) => (true, rest.to_owned()),
                None => (false, output),
            };
            validate_pattern(&pattern)?;
            if negated {
                negations.push(Pattern::new(&pattern, false)?);
                continue;
            }
            let (anchor, glob) = crate::ignored::partition(&pattern);
            // The path a literal output names, its escapes read, is what a
            // restore replaces, so it must not hold a project.
            if anchor.is_empty()
                || glob.is_none()
                    && workspace.projects.values().any(|project| {
                        project.root == anchor
                            || project
                                .root
                                .strip_prefix(anchor.as_str())
                                .is_some_and(|rest| rest.starts_with('/'))
                    })
            {
                bail!(
                    "cache outputs must have a fixed directory prefix and cannot replace a project root"
                );
            }
            if glob.is_none() {
                complete.insert(anchor.clone());
            }
            patterns.push(Pattern::new(&pattern, false)?);
            globs.push(pattern);
            anchors.insert(anchor);
        }
        Ok(Self {
            patterns,
            globs,
            negations,
            anchors,
            complete,
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

    /// The patterns as written, without the negated ones.
    pub(crate) fn globs(&self) -> &[String] {
        &self.globs
    }

    /// Fixed directory prefixes; every matching path lies below one of them.
    pub fn anchors(&self) -> impl Iterator<Item = &str> {
        self.anchors.iter().map(String::as_str)
    }

    /// Complete literal roots only: globs, implicit defaults and any exclusion
    /// retain per-artifact restoration. Nested roots are handled by their parent.
    pub(crate) fn complete_roots(&self) -> Vec<&str> {
        if !self.explicit || !self.negations.is_empty() {
            return Vec::new();
        }
        let mut roots: Vec<&str> = Vec::new();
        for root in &self.complete {
            if !roots.iter().any(|parent| {
                root.strip_prefix(parent)
                    .is_some_and(|suffix| suffix.starts_with('/'))
            }) {
                roots.push(root);
            }
        }
        roots
    }

    pub fn matches(&self, path: &str) -> bool {
        let any = |patterns: &[Pattern]| {
            Path::new(path)
                .ancestors()
                .any(|ancestor| patterns.iter().any(|pattern| pattern.is_match(ancestor)))
        };
        (self.complete.iter().any(|root| {
            path == root
                || path
                    .strip_prefix(root)
                    .is_some_and(|suffix| suffix.starts_with('/'))
        }) || any(&self.patterns))
            && !any(&self.negations)
    }

    pub fn paths(&self, root: &Path) -> Result<BTreeSet<String>> {
        self.listing(root).map(|(paths, _)| paths)
    }

    pub(crate) fn entries<'a>(&self, root: &'a Path) -> Result<OutputEntries<'a>> {
        if self.explicit && self.negations.is_empty() && self.complete.len() == self.globs.len() {
            let mut walks = Vec::new();
            let mut symlink = false;
            for anchor in &self.anchors {
                safe_parents(root, anchor)?;
                match std::fs::symlink_metadata(root.join(anchor)) {
                    Ok(metadata) => symlink |= metadata.file_type().is_symlink(),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
            for anchor in self.complete_roots() {
                let absolute = root.join(anchor);
                match std::fs::symlink_metadata(&absolute) {
                    Ok(_) => walks.push(walkdir::WalkDir::new(absolute).follow_links(false)),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
            if !symlink {
                return Ok(Box::new(walks.into_iter().flatten().map(move |entry| {
                    let entry = entry?;
                    let path = relative(root, entry.path())?;
                    validate_path(&path)?;
                    Ok((path, entry.metadata()?))
                })));
            }
        }
        let (paths, mut traversal) = self.listing(root)?;
        Ok(Box::new(paths.into_iter().map(move |path| {
            let absolute = root.join(&path);
            let metadata = match traversal.metadata.remove(&absolute) {
                Some(metadata) => metadata,
                None => std::fs::symlink_metadata(&absolute)?,
            };
            Ok((path, metadata))
        })))
    }

    fn listing(&self, root: &Path) -> Result<(BTreeSet<String>, Traversal)> {
        if let Some(segments) = self.simple_segments()? {
            for anchor in &self.anchors {
                safe_parents(root, anchor)?;
                match std::fs::symlink_metadata(root.join(anchor)) {
                    Ok(metadata) if metadata.file_type().is_symlink() => {
                        return self
                            .walk_paths(root)
                            .map(|paths| (paths, Traversal::default()));
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
            let mut traversal = Traversal::default();
            let mut candidates = BTreeSet::new();
            for (glob, pattern) in self.globs.iter().zip(&segments) {
                let fixed = pattern
                    .iter()
                    .take_while(|segment| matches!(segment, Segment::Literal(_)))
                    .count();
                let anchor = glob.split('/').take(fixed).collect::<Vec<_>>().join("/");
                traversal.expand(root, &root.join(anchor), &pattern[fixed..], &mut candidates)?;
            }
            return candidates
                .into_iter()
                .filter(|path| self.matches(path))
                .map(|path| {
                    validate_path(&path)?;
                    Ok(path)
                })
                .collect::<Result<_>>()
                .map(|paths| (paths, traversal));
        }
        self.walk_paths(root)
            .map(|paths| (paths, Traversal::default()))
    }

    fn simple_segments(&self) -> Result<Option<Vec<Vec<Segment>>>> {
        if self.globs.iter().any(|glob| {
            glob.contains("**")
                || glob.contains(['!', '@', '+', '|', ',', '{', '}', '[', ']', '(', ')'])
        }) {
            return Ok(None);
        }
        let mut matchers: BTreeMap<String, Pattern> = BTreeMap::new();
        let mut patterns = Vec::new();
        for glob in &self.globs {
            let mut segments = Vec::new();
            for part in glob.split('/') {
                segments.push(if crate::glob::is_literal(part) {
                    Segment::Literal(part.to_owned())
                } else {
                    let matcher = match matchers.entry(part.to_owned()) {
                        std::collections::btree_map::Entry::Occupied(entry) => entry.get().clone(),
                        std::collections::btree_map::Entry::Vacant(entry) => {
                            entry.insert(Pattern::new(part, false)?).clone()
                        }
                    };
                    Segment::Glob(matcher)
                });
            }
            patterns.push(segments);
        }
        Ok(Some(patterns))
    }

    fn walk_paths(&self, root: &Path) -> Result<BTreeSet<String>> {
        let mut paths = BTreeSet::new();
        for anchor in &self.anchors {
            safe_parents(root, anchor)?;
            let path = root.join(anchor);
            match std::fs::symlink_metadata(&path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
                Ok(_) => {}
            }
            let walk = walkdir::WalkDir::new(path)
                .follow_links(false)
                .into_iter()
                .filter_entry(|entry| {
                    entry.depth() == 0
                        || !entry.file_type().is_dir()
                        || relative(root, entry.path()).map_or(true, |directory| {
                            self.globs
                                .iter()
                                .any(|glob| may_match_below(glob, &directory))
                        })
                });
            for entry in walk {
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

#[derive(Clone)]
enum Segment {
    Literal(String),
    Glob(Pattern),
}

#[derive(Default)]
struct Traversal {
    metadata: BTreeMap<PathBuf, std::fs::Metadata>,
    directories: BTreeMap<PathBuf, Vec<PathBuf>>,
}

impl Traversal {
    fn metadata(&mut self, path: &Path) -> Result<Option<std::fs::Metadata>> {
        if let Some(metadata) = self.metadata.get(path) {
            return Ok(Some(metadata.clone()));
        }
        match std::fs::symlink_metadata(path) {
            Ok(metadata) => {
                self.metadata.insert(path.to_owned(), metadata.clone());
                Ok(Some(metadata))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn children(&mut self, path: &Path) -> Result<&Vec<PathBuf>> {
        if !self.directories.contains_key(path) {
            let children = std::fs::read_dir(path)?
                .map(|entry| entry.map(|entry| entry.path()))
                .collect::<std::io::Result<Vec<_>>>()?;
            self.directories.insert(path.to_owned(), children);
        }
        Ok(&self.directories[path])
    }

    fn expand(
        &mut self,
        root: &Path,
        path: &Path,
        segments: &[Segment],
        paths: &mut BTreeSet<String>,
    ) -> Result<()> {
        let Some(metadata) = self.metadata(path)? else {
            return Ok(());
        };
        let Some((segment, rest)) = segments.split_first() else {
            for entry in walkdir::WalkDir::new(path)
                .follow_links(false)
                .follow_root_links(false)
            {
                paths.insert(relative(root, entry?.path())?);
            }
            return Ok(());
        };
        if !metadata.is_dir() {
            return Ok(());
        }
        match segment {
            Segment::Literal(name) => {
                let candidate = path.join(name);
                if self.metadata(&candidate)?.is_none() {
                    return Ok(());
                }
                // A successful lookup may differ in case on the host filesystem.
                let child = self
                    .children(path)?
                    .iter()
                    .find(|child| child.file_name() == Some(std::ffi::OsStr::new(name)))
                    .cloned();
                if let Some(child) = child {
                    self.expand(root, &child, rest, paths)?;
                }
                Ok(())
            }
            Segment::Glob(matcher) => {
                let children: Vec<_> = self
                    .children(path)?
                    .iter()
                    .filter(|child| matcher.is_match(child.file_name().unwrap()))
                    .cloned()
                    .collect();
                for child in children {
                    self.expand(root, &child, rest, paths)?;
                }
                Ok(())
            }
        }
    }
}

/// Whether `glob` can match `directory` or a path below it. A path matches
/// when it or an ancestor does, so a directory deeper than the glob counts.
/// Groups and character classes can contain slashes, so a glob with one is
/// not ruled out by comparing slash-separated segments.
fn may_match_below(glob: &str, directory: &str) -> bool {
    if glob.contains(['{', '}', '(', ')', '[', ']']) {
        return true;
    }
    for (part, name) in glob.split('/').zip(directory.split('/')) {
        if part == "**" {
            return true;
        }
        if crate::glob::is_literal(part) && part != name {
            return false;
        }
    }
    true
}

/// A task's outputs as Nx lists them: workspace-relative paths and globs,
/// `!` for a negated one, with their tokens read as [`interpolate_output`]
/// reads them; an output naming what has no value is left out. Without
/// `outputs`, Nx's defaults: `options.outputPath`, and for `build` and
/// `prepare` targets `dist/<root>`, `<root>/dist`, `<root>/build` and
/// `<root>/public`. The flag tells whether the target declared them.
pub fn resolved_outputs(workspace: &Workspace, task: &Task) -> Result<(Vec<String>, bool)> {
    let root = &workspace.projects[&task.project].root;
    let options = task_options(task);
    let mut resolved = Vec::new();
    let Some(templates) = &task.definition.outputs else {
        // As in Nx, an `outputPath` counts when it is truthy, so an empty
        // string falls back to the defaults; an empty path in a list names
        // nothing.
        let defaults: Vec<String> = match options.get("outputPath") {
            Some(path @ serde_json::Value::String(text)) if !text.is_empty() => {
                vec![option_tokens(workspace, &task.project, path)?]
            }
            Some(serde_json::Value::Array(paths)) => paths
                .iter()
                .filter(|path| path.as_str().is_some_and(|text| !text.is_empty()))
                .map(|path| option_tokens(workspace, &task.project, path))
                .collect::<Result<_>>()?,
            _ if matches!(task.target.as_str(), "build" | "prepare") => {
                // The root goes in escaped, as in a configured output.
                let root = crate::glob::escape(root);
                [
                    format!("dist/{root}"),
                    format!("{root}/dist"),
                    format!("{root}/build"),
                    format!("{root}/public"),
                ]
                .into()
            }
            _ => Vec::new(),
        };
        for output in defaults {
            let (negated, path) = match output.strip_prefix('!') {
                Some(rest) => (true, rest),
                None => (false, output.as_str()),
            };
            let path = workspace_relative(workspace, path)?;
            resolved.push(if negated { format!("!{path}") } else { path });
        }
        return Ok((resolved, false));
    };
    qk_taskgraph::validate_outputs(templates)?;
    let data = OutputData {
        workspace,
        project: &task.project,
        options: &options,
    };
    for template in templates {
        let (negated, template) = match template.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, template.as_str()),
        };
        let Some(output) = interpolate_output(template, &data)? else {
            continue;
        };
        let path = workspace_relative(workspace, &output)?;
        let path = if negated { format!("!{path}") } else { path };
        // As in Nx, outputs are a set.
        if !resolved.contains(&path) {
            resolved.push(path);
        }
    }
    Ok((resolved, true))
}

/// `path` without Windows' verbatim `\\?\` prefix on a drive path.
fn verbatim_free(path: &Path) -> PathBuf {
    match path.to_str().and_then(|text| text.strip_prefix(r"\\?\")) {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => path.to_path_buf(),
    }
}

/// An output made workspace-relative, as Nx's cache reads one: an absolute
/// path inside the workspace is relativized, and one outside it, or a path
/// that climbs out with `..`, is an error.
fn workspace_relative(workspace: &Workspace, output: &str) -> Result<String> {
    let owned;
    let relative = if Path::new(output).is_absolute() || output.starts_with('/') {
        // Compared by components, without Windows' `\\?\` prefix, which
        // one of the two paths may carry and the other not.
        let inside = verbatim_free(Path::new(output))
            .strip_prefix(verbatim_free(&workspace.root))
            .ok()
            .map(|rest| {
                rest.components()
                    .map(|part| part.as_os_str().to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("/")
            });
        match inside {
            Some(rest) => {
                owned = rest;
                owned.as_str()
            }
            None => bail!("cache output is outside the workspace: {output}"),
        }
    } else {
        output
    };
    match normalize(relative) {
        Ok(path) => Ok(path.trim_end_matches('/').to_owned()),
        Err(_) => bail!("cache output is outside the workspace: {output}"),
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

/// What an output's tokens read: as in Nx, `{projectRoot}`, `{projectName}`,
/// `{options.*}` and the legacy `{project.*}`, the project's configuration.
struct OutputData<'a> {
    workspace: &'a Workspace,
    project: &'a str,
    options: &'a serde_json::Map<String, serde_json::Value>,
}

impl OutputData<'_> {
    fn is_token(name: &str) -> bool {
        matches!(name, "projectRoot" | "projectName" | "project" | "options")
    }

    fn read(&self, name: &str, path: Option<&str>) -> Result<Option<serde_json::Value>> {
        let value = match name {
            "projectRoot" => {
                serde_json::Value::String(self.workspace.projects[self.project].root.clone())
            }
            "projectName" => serde_json::Value::String(self.project.to_owned()),
            "project" => {
                let mut project = serde_json::to_value(&self.workspace.projects[self.project])?;
                project["name"] = serde_json::Value::String(self.project.to_owned());
                project
            }
            _ => serde_json::Value::Object(self.options.clone()),
        };
        let Some(found) = read_path(&value, path) else {
            return Ok(None);
        };
        // Nx resolves the project tokens in every option when it builds the
        // project graph, so an option's value already has them replaced. The
        // value goes in as written, a pattern if it holds one, as in Nx. The
        // project's own paths go in with their glob characters escaped, so a
        // root such as `apps/(group)` names that directory; Nx inserts them
        // as they are.
        Ok(Some(if name == "options" {
            resolve_option_tokens(self.workspace, self.project, &found)?
        } else {
            escape_strings(&found)
        }))
    }
}

/// `value` with every string in it escaped by [`crate::glob::escape`].
fn escape_strings(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::String(text) => serde_json::Value::String(crate::glob::escape(text)),
        serde_json::Value::Array(items) => items.iter().map(escape_strings).collect(),
        serde_json::Value::Object(object) => serde_json::Value::Object(
            object
                .iter()
                .map(|(key, item)| (key.clone(), escape_strings(item)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Walks a dotted path as Nx's `readPath` does: each step must be an own
/// property, so an object's key, or an array's or string's canonical index
/// (`0`, `12`, not `01` or `+1`) or `length`.
fn read_path(value: &serde_json::Value, path: Option<&str>) -> Option<serde_json::Value> {
    let Some(path) = path else {
        return Some(value.clone());
    };
    let index = |key: &str| {
        (key == "0" || (!key.starts_with('0') && key.bytes().all(|b| b.is_ascii_digit())))
            .then(|| key.parse::<usize>().ok())
            .flatten()
    };
    let mut value = value.clone();
    for key in path.split('.') {
        value = match &value {
            serde_json::Value::Object(object) => object.get(key)?.clone(),
            serde_json::Value::Array(items) if key == "length" => items.len().into(),
            serde_json::Value::Array(items) => items.get(index(key)?)?.clone(),
            // A JavaScript string's length and indexes count UTF-16 units.
            serde_json::Value::String(text) => {
                let units: Vec<u16> = text.encode_utf16().collect();
                if key == "length" {
                    units.len().into()
                } else {
                    String::from_utf16_lossy(&[*units.get(index(key)?)?]).into()
                }
            }
            _ => return None,
        };
    }
    Some(value)
}

/// An option value as Nx's graph holds it, from [`resolve_option_tokens`],
/// as the text it stands for.
fn option_tokens(
    workspace: &Workspace,
    project: &str,
    value: &serde_json::Value,
) -> Result<String> {
    Ok(text(&resolve_option_tokens(workspace, project, value)?).unwrap_or_default())
}

/// `{projectRoot}`, `{projectName}` and a leading `{workspaceRoot}` replaced
/// in every string of an option value, as Nx's `resolveNxTokensInOptions`:
/// `{workspaceRoot}` anywhere but the start is an error.
fn resolve_option_tokens(
    workspace: &Workspace,
    project: &str,
    value: &serde_json::Value,
) -> Result<serde_json::Value> {
    Ok(match value {
        serde_json::Value::String(text) => {
            let text = text
                .strip_prefix("{workspaceRoot}/")
                .or_else(|| text.strip_prefix("{workspaceRoot}"))
                .unwrap_or(text);
            if text.contains("{workspaceRoot}") {
                bail!(
                    "the {{workspaceRoot}} token is only valid at the beginning of an option ({project})"
                );
            }
            serde_json::Value::String(
                text.replace("{projectRoot}", &workspace.projects[project].root)
                    .replace("{projectName}", project),
            )
        }
        serde_json::Value::Array(items) => serde_json::Value::Array(
            items
                .iter()
                .map(|item| resolve_option_tokens(workspace, project, item))
                .collect::<Result<_>>()?,
        ),
        serde_json::Value::Object(object) => serde_json::Value::Object(
            object
                .iter()
                .map(|(key, item)| {
                    Ok((
                        key.clone(),
                        resolve_option_tokens(workspace, project, item)?,
                    ))
                })
                .collect::<Result<_>>()?,
        ),
        other => other.clone(),
    })
}

/// A value as JavaScript's `String()` writes it, or `None` for one Nx counts
/// as unset: missing, `null` or `''`.
fn text(value: &serde_json::Value) -> Option<String> {
    let text = match value {
        serde_json::Value::Null => return None,
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Bool(flag) => flag.to_string(),
        serde_json::Value::Number(number) => match number.as_f64() {
            Some(float) if number.is_f64() && float.fract() == 0.0 && float.abs() < 1e21 => {
                format!("{float:.0}")
            }
            _ => number.to_string(),
        },
        serde_json::Value::Array(items) => items
            .iter()
            .map(|item| text(item).unwrap_or_default())
            .collect::<Vec<_>>()
            .join(","),
        serde_json::Value::Object(_) => "[object Object]".to_owned(),
    };
    (!text.is_empty()).then_some(text)
}

/// One piece of a template: text, or a `{token}` with its `??` fallbacks.
enum Node<'t> {
    Text(String),
    Token { raw: &'t str, parts: Vec<Part<'t>> },
}

enum Part<'t> {
    Reference {
        name: &'t str,
        path: Option<&'t str>,
    },
    Literal(Vec<Node<'t>>),
}

/// A template's pieces, as Nx's `parseTokens`: braces that do not start
/// with a token are text, and a token can still sit inside them.
fn parse_tokens(template: &str) -> Result<Vec<Node<'_>>> {
    let mut nodes = Vec::new();
    let mut text = String::new();
    let mut index = 0;
    while let Some(offset) = template[index..].find('{') {
        let open = index + offset;
        let token = match qk_executor::tokens::closing_brace(template, open) {
            Some(close) => parse_token(&template[open..=close])?.map(|token| (token, close)),
            None => None,
        };
        let Some((token, close)) = token else {
            text.push_str(&template[index..=open]);
            index = open + 1;
            continue;
        };
        text.push_str(&template[index..open]);
        if !text.is_empty() {
            nodes.push(Node::Text(std::mem::take(&mut text)));
        }
        nodes.push(token);
        index = close + 1;
    }
    text.push_str(&template[index..]);
    if !text.is_empty() {
        nodes.push(Node::Text(text));
    }
    Ok(nodes)
}

/// A `{...}` as a token, or `None` when its first part names no token. Only
/// the last fallback may be a literal, which can hold tokens itself. `env.`
/// is not a token in an output, so it is text there.
fn parse_token(raw: &str) -> Result<Option<Node<'_>>> {
    let sources = qk_executor::tokens::fallbacks(&raw[1..raw.len() - 1]);
    fn reference(source: &str) -> Option<(&str, Option<&str>)> {
        let (name, path) = match source.split_once('.') {
            Some((name, path)) => (name, Some(path)),
            None => (source, None),
        };
        OutputData::is_token(name).then_some((name, path))
    }
    if reference(sources[0]).is_none() {
        return Ok(None);
    }
    let mut parts = Vec::new();
    for (index, source) in sources.iter().enumerate() {
        parts.push(match reference(source) {
            Some((name, path)) => Part::Reference { name, path },
            None if index + 1 == sources.len() => Part::Literal(parse_tokens(source)?),
            None => {
                bail!("{raw:?} has a fallback after the literal {source:?} that can never be used")
            }
        });
    }
    Ok(Some(Node::Token { raw, parts }))
}

/// Whether `{workspaceRoot}` appears only at the very start, which is the
/// only place Nx allows it in an output. A fallback's literal replaces its
/// whole token, so a token at the start may fall back to one.
fn workspace_root_only_at_start(nodes: &[Node], at_start: bool) -> bool {
    nodes.iter().enumerate().all(|(index, node)| {
        let node_at_start = at_start && index == 0;
        match node {
            Node::Text(text) => {
                let skip = usize::from(node_at_start).min(text.len());
                !text[skip..].contains("{workspaceRoot}")
            }
            Node::Token { parts, .. } => parts.iter().all(|part| match part {
                Part::Literal(nodes) => workspace_root_only_at_start(nodes, node_at_start),
                Part::Reference { .. } => true,
            }),
        }
    })
}

/// What an output names once its tokens are read, as Nx 24's
/// `interpolateOutput`, or `None` when Nx leaves it out: it is empty, or a
/// `{projectRoot}`, `{workspaceRoot}` or `{options...}` placeholder was left
/// without a value. A token's value is its first part that is set; braces
/// that start no token, such as `*.{js,map}`, stay as written. The result is
/// joined as a path, and a leading `{workspaceRoot}/` removed. An output
/// starting with `/` is taken as written.
fn interpolate_output(template: &str, data: &OutputData) -> Result<Option<String>> {
    let output = if template.starts_with('/') || !has_braces(template) {
        template.to_owned()
    } else {
        let nodes = parse_tokens(template)?;
        if !workspace_root_only_at_start(&nodes, true) {
            bail!(
                "output {template:?} is invalid: {{workspaceRoot}} can only be used at the beginning of the expression"
            );
        }
        let mut segments = vec![String::new()];
        render(&nodes, data, false, &mut segments)?;
        // `path.join` skips empty segments.
        segments.retain(|segment| !segment.is_empty());
        join_path(&segments.join("/")).replacen("{workspaceRoot}/", "", 1)
    };
    let unresolved = ["{projectRoot}", "{workspaceRoot}"]
        .iter()
        .any(|token| output.contains(token))
        || output
            .find("{options")
            .is_some_and(|start| output[start..].contains('}'));
    Ok((!output.is_empty() && !unresolved).then_some(output))
}

/// Whether `template` holds a `{` with something before a later `}`.
fn has_braces(template: &str) -> bool {
    template.find('{').is_some_and(|open| {
        template
            .get(open + 2..)
            .is_some_and(|rest| rest.contains('}'))
    })
}

/// The template's `/`-separated segments with its tokens read, as Nx's
/// `joinSegments` gathers them for `path.join`. Text splits segments and a
/// value joins the segment it lands in, so only a value or a fallback's
/// literal starting a path can make it absolute.
fn render(
    nodes: &[Node],
    data: &OutputData,
    literal: bool,
    segments: &mut Vec<String>,
) -> Result<()> {
    for (index, node) in nodes.iter().enumerate() {
        match node {
            Node::Text(text) => {
                let root = if literal && index == 0 && text.starts_with('/') {
                    "/"
                } else {
                    ""
                };
                let mut parts = text[root.len()..].split('/');
                let last = segments.last_mut().expect("segments start with one");
                last.push_str(root);
                last.push_str(parts.next().unwrap_or_default());
                segments.extend(parts.map(str::to_owned));
            }
            Node::Token { raw, parts } => {
                // A root project's `{projectRoot}` is no path segment at all.
                if *raw == "{projectRoot}" && data.workspace.projects[data.project].root == "." {
                    continue;
                }
                let mut value = Some(*raw);
                let mut found = None;
                for part in parts {
                    match part {
                        Part::Literal(nodes) => {
                            render(nodes, data, true, segments)?;
                            value = None;
                            break;
                        }
                        Part::Reference { name, path } => {
                            found = data.read(name, *path)?.as_ref().and_then(text);
                            if found.is_some() {
                                break;
                            }
                        }
                    }
                }
                // A token with no value stays as written.
                let value = found.as_deref().or(value).unwrap_or_default();
                segments
                    .last_mut()
                    .expect("segments start with one")
                    .push_str(value);
            }
        }
    }
    Ok(())
}

/// `path` joined as Node's `path.posix.join` joins it: empty and `.`
/// segments dropped, `..` applied where it can be, a trailing slash kept.
fn join_path(path: &str) -> String {
    let absolute = path.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => match parts.last() {
                Some(&last) if last != ".." => {
                    parts.pop();
                }
                _ if absolute => {}
                _ => parts.push(".."),
            },
            part => parts.push(part),
        }
    }
    let mut joined = parts.join("/");
    if absolute {
        joined.insert(0, '/');
    }
    if joined.is_empty() {
        joined.push('.');
    }
    if path.ends_with('/') && !joined.ends_with('/') {
        joined.push('/');
    }
    joined
}

#[cfg(test)]
mod tests {
    use super::{Outputs, Pattern, dot_git, normalize, resolved_outputs, verbatim_free};
    use qk_config::Workspace;
    use qk_taskgraph::{Request, TaskGraph};
    use serde_json::{Value, json};
    use std::path::{Path, PathBuf};

    /// The outputs of `lib:build` in a workspace with a root project and
    /// `lib` at `libs/lib`, the target configured by `target`.
    fn outputs_of(target: Value, args: &[&str]) -> anyhow::Result<Vec<String>> {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("nx.json"), "{}").unwrap();
        std::fs::write(
            root.path().join("project.json"),
            json!({"name": "root"}).to_string(),
        )
        .unwrap();
        std::fs::create_dir_all(root.path().join("libs/lib")).unwrap();
        std::fs::write(
            root.path().join("libs/lib/project.json"),
            json!({"name": "lib", "sourceRoot": "libs/lib/src", "tags": ["a", "b"],
                "targets": {"build": target}})
            .to_string(),
        )
        .unwrap();
        let workspace = Workspace::load(root.path()).unwrap();
        let mut request = Request::parse("lib:build").unwrap();
        request.args = args.iter().map(|arg| (*arg).to_owned()).collect();
        let graph = TaskGraph::build(&workspace, &[request])?;
        resolved_outputs(&workspace, &graph.tasks["lib:build"]).map(|(outputs, _)| outputs)
    }

    fn outputs(outputs: Value, options: Value) -> anyhow::Result<Vec<String>> {
        outputs_of(
            json!({"command": "true", "outputs": outputs, "options": options}),
            &[],
        )
    }

    #[test]
    fn braces_that_are_no_token_stay_as_written() {
        assert_eq!(
            outputs(
                json!([
                    "{projectRoot}/dist/**/*.{js,map}",
                    "{workspaceRoot}/x/{a,{projectName}}"
                ]),
                json!({})
            )
            .unwrap(),
            ["libs/lib/dist/**/*.{js,map}", "x/{a,lib}"]
        );
    }

    #[test]
    fn placeholders_fall_back_left_to_right() {
        let resolved = outputs(
            json!([
                "{options.a ?? options.b ?? dist/{projectName}}",
                "{options.missing ?? options.b}/x",
                "{options.missing ?? {workspaceRoot}/fallback}",
                "{workspaceRoot}/x/{env.HOME}",
                "{options.missing}",
                "{options.missing ?? options.other}",
            ]),
            json!({"b": "from-b"}),
        )
        .unwrap();
        // `env.` is no token in an output, and an output with an unresolved
        // placeholder is left out.
        assert_eq!(resolved, ["from-b", "from-b/x", "fallback", "x/{env.HOME}"]);
        assert!(outputs(json!(["{options.a ?? literal ?? options.b}"]), json!({})).is_err());
    }

    #[test]
    fn option_values_read_as_javascript_writes_them() {
        assert_eq!(
            outputs(
                json!([
                    "{workspaceRoot}/dist/{options.flag}",
                    "{workspaceRoot}/dist/{options.zero}",
                    "{workspaceRoot}/dist/{options.list}",
                    "{workspaceRoot}/dist/{options.float}",
                    "{workspaceRoot}/dist/{options.nested.deep}",
                    "{workspaceRoot}/dist/{options.list.1}",
                    "{workspaceRoot}/dist/{options.empty}",
                    "{workspaceRoot}/dist/{options.none}",
                ]),
                json!({"flag": false, "zero": 0, "list": ["a", "b"], "float": 2.0,
                    "nested": {"deep": "d"}, "empty": "", "none": null}),
            )
            .unwrap(),
            [
                "dist/false",
                "dist/0",
                "dist/a,b",
                "dist/2",
                "dist/d",
                "dist/b"
            ]
        );
    }

    #[test]
    fn option_values_have_their_project_tokens_resolved() {
        assert_eq!(
            outputs(
                json!(["{options.outputPath}", "{options.list}"]),
                json!({"outputPath": "{workspaceRoot}/dist/{projectName}/{projectRoot}",
                    "list": ["{projectName}"]}),
            )
            .unwrap(),
            ["dist/lib/libs/lib", "lib"]
        );
        // `{workspaceRoot}` elsewhere in an option fails when the workspace
        // loads, as in Nx; see qk-config's workspace tests.
        // Arguments override options and are taken as they are given.
        assert_eq!(
            outputs_of(
                json!({"command": "true", "outputs": ["{options.out}"], "options": {"out": "a"}}),
                &["--out=b"],
            )
            .unwrap(),
            ["b"]
        );
        // Paths step through own properties as Nx's readPath does: an array's
        // length and canonical indexes only.
        assert_eq!(
            outputs(
                json!([
                    "{options.list.length}",
                    "{options.list.1}",
                    "{options.list.01}"
                ]),
                json!({"list": ["a", "b"]}),
            )
            .unwrap(),
            ["2", "b"]
        );
        // An empty `outputPath` falls back to a build target's defaults, as
        // Nx's truthiness check does.
        assert_eq!(
            outputs_of(
                json!({"command": "true", "options": {"outputPath": ""}}),
                &[],
            )
            .unwrap(),
            [
                "dist/libs/lib",
                "libs/lib/dist",
                "libs/lib/build",
                "libs/lib/public"
            ]
        );
        assert_eq!(
            outputs_of(
                json!({"command": "true", "options": {"outputPath": ["", "dist"]}}),
                &[],
            )
            .unwrap(),
            ["dist"]
        );
        // Without `outputs`, `outputPath` is the output, its tokens resolved.
        assert_eq!(
            outputs_of(
                json!({"command": "true", "options": {"outputPath": "dist/{projectName}"}}),
                &[],
            )
            .unwrap(),
            ["dist/lib"]
        );
    }

    #[test]
    fn legacy_project_paths_read_the_project_configuration() {
        assert_eq!(
            outputs(
                json!([
                    "{project.sourceRoot}/gen",
                    "{workspaceRoot}/dist/{project.name}/{project.tags}",
                    "{project.missing}/x"
                ]),
                json!({})
            )
            .unwrap(),
            ["libs/lib/src/gen", "dist/lib/a,b", "{project.missing}/x"]
        );
    }

    #[test]
    fn workspace_root_outputs_name_a_directory() {
        for output in ["{workspaceRoot}/**", "{workspaceRoot}/*.js"] {
            assert!(outputs(json!([output]), json!({})).is_err(), "{output}");
        }
        assert_eq!(
            outputs(json!(["{workspaceRoot}/@scope/**"]), json!({})).unwrap(),
            ["@scope/**"]
        );
    }

    #[test]
    fn workspace_root_is_allowed_only_at_the_start() {
        for output in ["{projectRoot}/{workspaceRoot}/dist", "dist/{workspaceRoot}"] {
            assert!(outputs(json!([output]), json!({})).is_err(), "{output}");
        }
        assert_eq!(
            outputs(
                json!([
                    "!{workspaceRoot}/dist/x",
                    "{workspaceRoot}/dist",
                    "{workspaceRoot}"
                ]),
                json!({})
            )
            .unwrap(),
            ["!dist/x", "dist"]
        );
    }

    #[test]
    fn a_root_projects_root_is_no_segment() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("nx.json"), "{}").unwrap();
        std::fs::write(
            root.path().join("project.json"),
            json!({"name": "app", "targets": {"build": {"command": "true",
                "outputs": ["{projectRoot}/dist", "{options.out ?? {projectRoot}/out}"]}}})
            .to_string(),
        )
        .unwrap();
        let workspace = Workspace::load(root.path()).unwrap();
        let graph = TaskGraph::build(&workspace, &[Request::parse("app:build").unwrap()]).unwrap();
        assert_eq!(
            resolved_outputs(&workspace, &graph.tasks["app:build"])
                .unwrap()
                .0,
            ["dist", "out"]
        );
    }

    #[test]
    fn verbatim_prefixes_are_left_out_of_drive_paths_only() {
        assert_eq!(
            verbatim_free(Path::new(r"\\?\C:\repo\dist")),
            PathBuf::from(r"C:\repo\dist")
        );
        let unc = Path::new(r"\\?\UNC\server\share");
        assert_eq!(verbatim_free(unc), unc);
        assert_eq!(
            verbatim_free(Path::new("/repo/dist")),
            Path::new("/repo/dist")
        );
    }

    #[test]
    fn absolute_outputs_must_lie_in_the_workspace() {
        let root = tempfile::tempdir().unwrap();
        let inside = std::fs::canonicalize(root.path()).unwrap();
        std::fs::write(inside.join("nx.json"), "{}").unwrap();
        std::fs::write(
            inside.join("project.json"),
            json!({"name": "app", "targets": {"build": {"command": "true",
                "outputs": ["{options.out}"], "options": {"out": inside.join("dist/app")}},
                "outside": {"command": "true", "outputs": ["{options.out}"],
                    "options": {"out": "/elsewhere/dist"}}}})
            .to_string(),
        )
        .unwrap();
        let workspace = Workspace::load(&inside).unwrap();
        let graph = TaskGraph::build(
            &workspace,
            &[
                Request::parse("app:build").unwrap(),
                Request::parse("app:outside").unwrap(),
            ],
        )
        .unwrap();
        assert_eq!(
            resolved_outputs(&workspace, &graph.tasks["app:build"])
                .unwrap()
                .0,
            ["dist/app"]
        );
        assert!(resolved_outputs(&workspace, &graph.tasks["app:outside"]).is_err());
    }

    #[test]
    fn dot_git_finds_a_linked_worktrees_directories_without_git() {
        let scratch = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(scratch.path()).unwrap();
        let common = base.join("main/.git");
        let git_dir = common.join("worktrees/linked");
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::write(git_dir.join("commondir"), "../..\n").unwrap();
        std::fs::create_dir_all(base.join("linked/apps/web")).unwrap();
        std::fs::write(
            base.join("linked/.git"),
            "gitdir: ../main/.git/worktrees/linked\n",
        )
        .unwrap();
        assert_eq!(
            dot_git(&base.join("linked/apps/web")),
            Some((git_dir, common.clone()))
        );
        assert_eq!(dot_git(&base.join("main")), Some((common.clone(), common)));
        assert_eq!(dot_git(&base.join("elsewhere")), None);
    }

    #[test]
    fn literal_entries_match_listing_for_nested_and_missing_roots() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path();
        std::fs::create_dir_all(root.join("dist/nested/empty")).unwrap();
        std::fs::write(root.join("dist/nested/value"), "contents").unwrap();
        std::fs::write(root.join("single"), "one").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("single"), root.join("dist/link")).unwrap();
        let outputs = Outputs::from_paths(&[
            "dist".into(),
            "dist/nested".into(),
            "missing".into(),
            "single".into(),
        ])
        .unwrap();
        let entries = outputs
            .entries(root)
            .unwrap()
            .collect::<anyhow::Result<Vec<_>>>()
            .unwrap();
        let paths: std::collections::BTreeSet<_> =
            entries.iter().map(|(path, _)| path.clone()).collect();
        assert_eq!(entries.len(), paths.len());
        assert_eq!(paths, outputs.paths(root).unwrap());
        for (path, metadata) in entries {
            let expected = std::fs::symlink_metadata(root.join(path)).unwrap();
            assert_eq!(metadata.file_type(), expected.file_type());
            assert_eq!(metadata.len(), expected.len());
            assert_eq!(metadata.modified().unwrap(), expected.modified().unwrap());
        }
    }

    #[cfg(unix)]
    #[test]
    fn literal_entries_reject_nested_roots_below_a_symlink() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path();
        std::fs::create_dir_all(root.join("dist")).unwrap();
        std::fs::create_dir_all(root.join("outside/nested")).unwrap();
        std::os::unix::fs::symlink(root.join("outside"), root.join("dist/link")).unwrap();
        let outputs = Outputs::from_paths(&["dist".into(), "dist/link/nested".into()]).unwrap();
        assert!(outputs.entries(root).is_err());
    }

    /// A project's root and name go into its outputs as the paths they are;
    /// an option's value goes in as written.
    #[test]
    fn outputs_escape_the_project_root_and_name() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("nx.json"), "{}").unwrap();
        std::fs::create_dir_all(root.path().join("apps/(group)/dist")).unwrap();
        std::fs::write(root.path().join("apps/(group)/dist/a.js"), "").unwrap();
        std::fs::write(
            root.path().join("apps/(group)/project.json"),
            json!({"name": "a+b", "sourceRoot": "apps/(group)/src", "targets": {
                "build": {"command": "true"},
                "emit": {"command": "true", "options": {"out": "out/*.js"}, "outputs": [
                    "{projectRoot}/dist",
                    "{workspaceRoot}/dist/{projectName}",
                    "{project.sourceRoot}/gen",
                    "{options.out}",
                ]},
            }})
            .to_string(),
        )
        .unwrap();
        let workspace = Workspace::load(root.path()).unwrap();
        let graph = TaskGraph::build(
            &workspace,
            &[
                Request::parse("a+b:emit").unwrap(),
                Request::parse("a+b:build").unwrap(),
            ],
        )
        .unwrap();
        assert_eq!(
            resolved_outputs(&workspace, &graph.tasks["a+b:emit"])
                .unwrap()
                .0,
            [
                r"apps/\(group\)/dist",
                r"dist/a\+b",
                r"apps/\(group\)/src/gen",
                "out/*.js"
            ]
        );
        assert_eq!(
            resolved_outputs(&workspace, &graph.tasks["a+b:build"])
                .unwrap()
                .0,
            [
                r"dist/apps/\(group\)",
                r"apps/\(group\)/dist",
                r"apps/\(group\)/build",
                r"apps/\(group\)/public"
            ]
        );
        let outputs = Outputs::new(&workspace, &graph.tasks["a+b:emit"]).unwrap();
        assert!(
            outputs
                .paths(root.path())
                .unwrap()
                .contains("apps/(group)/dist/a.js")
        );
    }

    /// An output naming a project's root or a directory holding one, escaped
    /// or not, is refused: a restore would replace the project.
    #[test]
    fn outputs_cannot_name_a_project_root() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("nx.json"), "{}").unwrap();
        std::fs::create_dir_all(root.path().join("apps/(group)")).unwrap();
        std::fs::write(
            root.path().join("apps/(group)/project.json"),
            json!({"name": "group", "targets": {
                "own": {"command": "true", "outputs": ["{projectRoot}"]},
                "written": {"command": "true", "outputs": ["{workspaceRoot}/apps/\\(group\\)"]},
                "parent": {"command": "true", "outputs": ["{workspaceRoot}/apps"]},
                "sibling": {"command": "true", "outputs": ["{workspaceRoot}/apps/\\(gr"]},
            }})
            .to_string(),
        )
        .unwrap();
        let workspace = Workspace::load(root.path()).unwrap();
        for (target, refused) in [
            ("own", true),
            ("written", true),
            ("parent", true),
            ("sibling", false),
        ] {
            let request = Request::parse(&format!("group:{target}")).unwrap();
            let graph = TaskGraph::build(&workspace, &[request]).unwrap();
            assert_eq!(
                Outputs::new(&workspace, &graph.tasks[&format!("group:{target}")]).is_err(),
                refused,
                "{target}"
            );
        }
    }

    #[test]
    fn output_patterns_escape_glob_characters() {
        let workspace = tempfile::tempdir().unwrap();
        for file in [
            "app/(group)/page.js",
            "app/group/page.js",
            "dist/[id].js",
            "dist/i.js",
        ] {
            let path = workspace.path().join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
        let outputs =
            Outputs::from_paths(&[r"app/\(group\)".into(), r"dist/\[id\].js".into()]).unwrap();
        assert_eq!(
            outputs
                .paths(workspace.path())
                .unwrap()
                .into_iter()
                .collect::<Vec<_>>(),
            ["app/(group)", "app/(group)/page.js", "dist/[id].js"]
        );
        // An escaped first segment is the directory the outputs lie in.
        let outputs = Outputs::from_paths(&[r"\(group\)/dist".into()]).unwrap();
        std::fs::create_dir_all(workspace.path().join("(group)/dist")).unwrap();
        std::fs::write(workspace.path().join("(group)/dist/a.js"), "").unwrap();
        assert_eq!(outputs.anchors().collect::<Vec<_>>(), ["(group)/dist"]);
        assert_eq!(outputs.complete_roots(), ["(group)/dist"]);
        assert_eq!(
            outputs
                .paths(workspace.path())
                .unwrap()
                .into_iter()
                .collect::<Vec<_>>(),
            ["(group)/dist", "(group)/dist/a.js"]
        );
        for unsafe_pattern in [r"dist/\.\./x", r"dist\"] {
            assert!(
                Outputs::from_paths(&[unsafe_pattern.into()]).is_err(),
                "{unsafe_pattern}"
            );
        }
    }

    #[test]
    fn literal_roots_preserve_path_boundaries_and_exclusions() {
        let outputs =
            Outputs::from_paths(&["dist".into(), "build/*.js".into(), "!dist/private".into()])
                .unwrap();
        for path in ["dist", "dist/nested/file.txt", "build/app.js"] {
            assert!(outputs.matches(path), "{path}");
        }
        for path in [
            "dist2/file.txt",
            "dist/private",
            "dist/private/file.txt",
            "build/app.ts",
        ] {
            assert!(!outputs.matches(path), "{path}");
        }
    }

    /// Directories no glob can match are skipped without losing a match.
    #[test]
    fn listing_skips_only_directories_no_glob_reaches() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path();
        for file in [
            "apps/web/graphql/manifest.json",
            "apps/web/src/graphql/manifest.json",
            "apps/web/node_modules/pkg/graphql/manifest.json",
            "apps/client/app/graphql/manifest.json",
            "apps/client/app/graphql/nested/extra.json",
            "packages/a/deep/x/gen/out.ts",
            "packages/b/types/index.d.ts",
            "packages/c/lib/one.js",
        ] {
            let path = root.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
        let outputs = Outputs::from_paths(&[
            "apps/*/graphql/manifest.json".into(),
            "apps/*/app/graphql".into(),
            "packages/a/**/gen/*.ts".into(),
            "packages/b/types".into(),
            "packages/{c,d}/lib/*.js".into(),
        ])
        .unwrap();
        assert_eq!(
            outputs.paths(root).unwrap().into_iter().collect::<Vec<_>>(),
            [
                "apps/client/app/graphql",
                "apps/client/app/graphql/manifest.json",
                "apps/client/app/graphql/nested",
                "apps/client/app/graphql/nested/extra.json",
                "apps/web/graphql/manifest.json",
                "packages/a/deep/x/gen/out.ts",
                "packages/b/types",
                "packages/b/types/index.d.ts",
                "packages/c/lib/one.js",
            ]
        );
    }

    /// A slash inside a character class does not separate path segments.
    #[test]
    fn character_classes_with_slashes_keep_nested_artifacts() {
        let workspace = tempfile::tempdir().unwrap();
        for name in ["a", "b", "c"] {
            let directory = workspace.path().join(format!("apps/{name}/graphql/nested"));
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(directory.join("output.txt"), "value").unwrap();
        }
        let outputs = Outputs::from_paths(&["apps/[a/b]/graphql".into()]).unwrap();
        assert_eq!(
            outputs
                .paths(workspace.path())
                .unwrap()
                .into_iter()
                .collect::<Vec<_>>(),
            [
                "apps/a/graphql",
                "apps/a/graphql/nested",
                "apps/a/graphql/nested/output.txt",
                "apps/b/graphql",
                "apps/b/graphql/nested",
                "apps/b/graphql/nested/output.txt",
            ]
        );
    }

    #[test]
    fn segment_traversal_preserves_output_sets() {
        let workspace = tempfile::tempdir().unwrap();
        for file in [
            "apps/a/graphql/manifest.json",
            "apps/a/graphql/nested/out.ts",
            "apps/a/src/other.ts",
            "apps/b/src/graphql/manifest.json",
            "apps/.hidden/graphql/manifest.json",
            "apps/a/file",
            "dist/a.ts",
            "dist/deep/b.ts",
        ] {
            let path = workspace.path().join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "value").unwrap();
        }
        for patterns in [
            vec![
                "apps/*/graphql/manifest.json",
                "apps/*/*/graphql/manifest.json",
            ],
            vec!["apps/?/graphql", "!apps/a/graphql/nested"],
            vec!["apps/a/graphql", "apps/*/graphql/*"],
            vec!["apps/a/file/nested", "missing/*/file", "dist/*.ts"],
            vec!["dist"],
            vec!["apps/**/manifest.json"],
            vec!["apps/[a/b]/graphql"],
        ] {
            let outputs =
                Outputs::from_paths(&patterns.iter().map(|p| (*p).to_owned()).collect::<Vec<_>>())
                    .unwrap();
            match (
                outputs.paths(workspace.path()),
                outputs.walk_paths(workspace.path()),
            ) {
                (Ok(actual), Ok(expected)) => assert_eq!(actual, expected, "{patterns:?}"),
                (Err(actual), Err(expected)) => {
                    assert_eq!(actual.to_string(), expected.to_string())
                }
                results => panic!("different results for {patterns:?}: {results:?}"),
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn segment_traversal_preserves_symlink_boundaries() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path();
        std::fs::create_dir_all(root.join("apps/a/graphql")).unwrap();
        std::fs::write(root.join("apps/a/graphql/manifest.json"), "value").unwrap();
        std::os::unix::fs::symlink("a", root.join("apps/link")).unwrap();
        std::os::unix::fs::symlink("absent", root.join("apps/dangling")).unwrap();
        for patterns in [
            vec!["apps/*/graphql/manifest.json"],
            vec!["apps/*"],
            vec!["apps/link"],
            vec!["apps/dangling"],
        ] {
            let outputs =
                Outputs::from_paths(&patterns.iter().map(|p| (*p).to_owned()).collect::<Vec<_>>())
                    .unwrap();
            match (outputs.paths(root), outputs.walk_paths(root)) {
                (Ok(actual), Ok(expected)) => assert_eq!(actual, expected, "{patterns:?}"),
                (Err(actual), Err(expected)) => {
                    assert_eq!(actual.to_string(), expected.to_string())
                }
                results => panic!("different results for {patterns:?}: {results:?}"),
            }
        }
    }

    #[test]
    fn listings_recheck_metadata_and_directory_contents() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path();
        std::fs::create_dir_all(root.join("apps/a/graphql")).unwrap();
        std::fs::write(root.join("apps/a/graphql/manifest.json"), "one").unwrap();
        let outputs = Outputs::from_paths(&["apps/*/graphql/manifest.json".into()]).unwrap();
        assert_eq!(
            outputs
                .entries(root)
                .unwrap()
                .collect::<anyhow::Result<std::collections::BTreeMap<_, _>>>()
                .unwrap()["apps/a/graphql/manifest.json"]
                .len(),
            3
        );
        std::fs::write(root.join("apps/a/graphql/manifest.json"), "different").unwrap();
        std::fs::create_dir_all(root.join("apps/b/graphql")).unwrap();
        std::fs::write(root.join("apps/b/graphql/manifest.json"), "two").unwrap();
        let entries = outputs
            .entries(root)
            .unwrap()
            .collect::<anyhow::Result<std::collections::BTreeMap<_, _>>>()
            .unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries["apps/a/graphql/manifest.json"].len(), 9);
        std::fs::remove_dir_all(root.join("apps/a")).unwrap();
        assert_eq!(
            outputs
                .entries(root)
                .unwrap()
                .collect::<anyhow::Result<std::collections::BTreeMap<_, _>>>()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn literal_segments_after_wildcards_remain_case_sensitive() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path();
        std::fs::create_dir_all(root.join("apps/a/GraphQL")).unwrap();
        std::fs::write(root.join("apps/a/GraphQL/Manifest.json"), "one").unwrap();
        for glob in [
            "apps/*/graphql/Manifest.json",
            "apps/*/GraphQL/manifest.json",
            "apps/*/GraphQL/Manifest.json",
            "apps/a/GraphQL",
        ] {
            let outputs = Outputs::from_paths(&[glob.into()]).unwrap();
            assert_eq!(
                outputs.paths(root).unwrap(),
                outputs.walk_paths(root).unwrap(),
                "{glob}"
            );
        }
    }

    #[test]
    fn simple_glob_combinations_match_the_walker() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path();
        for project in ["a", "aa", "Ab", ".hidden", "café"] {
            for directory in ["graphql", "GraphQL", "src"] {
                let path = root.join(format!("apps/{project}/{directory}/nested"));
                std::fs::create_dir_all(&path).unwrap();
                std::fs::write(path.join("manifest.json"), "value").unwrap();
                std::fs::write(path.parent().unwrap().join("manifest.json"), "value").unwrap();
            }
        }
        for project in ["*", "?", "a*", "*a", "*?", "A*"] {
            for directory in ["graphql", "GraphQL", "src", "missing"] {
                for suffix in ["", "/manifest.json", "/*.json", "/?anifest.json", "/*"] {
                    let glob = format!("apps/{project}/{directory}{suffix}");
                    let outputs =
                        Outputs::from_paths(&[glob.clone(), "!apps/aa/graphql/nested".into()])
                            .unwrap();
                    assert_eq!(
                        outputs.paths(root).unwrap(),
                        outputs.walk_paths(root).unwrap(),
                        "{glob}"
                    );
                }
            }
        }
    }

    /// Only unconditional literal output roots may move as complete trees.
    #[test]
    fn complete_roots_do_not_widen_globs_exclusions_or_defaults() {
        let mut outputs = Outputs::from_paths(&[
            "dist".into(),
            "dist/nested".into(),
            "dist2".into(),
            "partial/**/*.js".into(),
        ])
        .unwrap();
        assert_eq!(outputs.complete_roots(), ["dist", "dist2"]);
        outputs
            .negations
            .push(Pattern::new("dist/private", false).unwrap());
        assert!(outputs.complete_roots().is_empty());
        outputs.negations.clear();
        outputs.explicit = false;
        assert!(outputs.complete_roots().is_empty());
    }

    #[test]
    fn normalizes_relative_segments() {
        assert_eq!(
            normalize("apps/client/tests/../build/*/bundle.bin").unwrap(),
            "apps/client/build/*/bundle.bin"
        );
        assert_eq!(normalize("./dist/./a").unwrap(), "dist/a");
        assert_eq!(normalize(".").unwrap(), ".");
        assert!(normalize("app/../../outside").is_err());
        assert!(normalize("app/**/../x").is_err());
        assert_eq!(normalize(r"apps/\(group\)/../x").unwrap(), "apps/x");
    }
}
