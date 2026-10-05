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

type OutputEntries<'a> = Box<dyn Iterator<Item = Result<(String, std::fs::Metadata)>> + 'a>;

#[derive(Clone)]
pub struct Outputs {
    patterns: Vec<Pattern>,
    /// The patterns as written, for skipping directories none can match.
    globs: Vec<String>,
    /// Negated patterns: what they match is never an output.
    negations: Vec<Pattern>,
    anchors: BTreeSet<String>,
    /// Literal output roots which include their entire subtree.
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
                validate_path(excluded)?;
                negations.push(Pattern::new(excluded, false)?);
                continue;
            }
            validate_path(pattern)?;
            let anchor = pattern
                .split('/')
                .take_while(|part| crate::glob::is_literal(part))
                .collect::<Vec<_>>()
                .join("/");
            if anchor.is_empty() {
                bail!("{pattern:?} needs a fixed directory prefix");
            }
            if pattern.split('/').all(crate::glob::is_literal) {
                complete.insert(pattern.clone());
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
            if pattern.split('/').all(crate::glob::is_literal) {
                complete.insert(pattern.clone());
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
/// `!` for a negated one, with `{options.name}`, `{projectName}` and the
/// project tokens replaced; an output naming what has no value is left out.
/// Without `outputs`, Nx's defaults: `options.outputPath`, and for `build`
/// and `prepare` targets `dist/<root>`, `<root>/dist`, `<root>/build` and
/// `<root>/public`. The flag tells whether the target declared them.
pub fn resolved_outputs(workspace: &Workspace, task: &Task) -> Result<(Vec<String>, bool)> {
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
    let mut resolved = Vec::new();
    for template in templates {
        let (negated, template) = match template.strip_prefix('!') {
            Some(rest) => (true, rest.to_owned()),
            None => (false, template),
        };
        let Some(output) = resolve_output(task, &options, &template) else {
            continue;
        };
        let path = expand(workspace, &task.project, &output)?
            .trim_end_matches('/')
            .to_owned();
        resolved.push(if negated { format!("!{path}") } else { path });
    }
    Ok((resolved, explicit))
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
    use super::{Outputs, Pattern, normalize};

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
            "apps/mobile/app/graphql/manifest.json",
            "apps/mobile/app/graphql/nested/extra.json",
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
                "apps/mobile/app/graphql",
                "apps/mobile/app/graphql/manifest.json",
                "apps/mobile/app/graphql/nested",
                "apps/mobile/app/graphql/nested/extra.json",
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
            normalize("apps/mobile/test-e2e/../ios/build/*/App.app").unwrap(),
            "apps/mobile/ios/build/*/App.app"
        );
        assert_eq!(normalize("./dist/./a").unwrap(), "dist/a");
        assert_eq!(normalize(".").unwrap(), ".");
        assert!(normalize("app/../../outside").is_err());
        assert!(normalize("app/**/../x").is_err());
    }
}
