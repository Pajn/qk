use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use ignore::WalkBuilder;
use serde::Deserialize;
use serde_json::Value;

use crate::minimatch::Minimatch;
use crate::{read_optional_json, relative_path};

/// Find the nearest workspace marker, including when invoked inside a project.
pub fn find_workspace(start: &Path) -> Result<PathBuf> {
    let start = start
        .canonicalize()
        .with_context(|| format!("cannot open {}", start.display()))?;
    let mut single_package = None;
    for directory in start.ancestors() {
        if directory.join("nx.json").is_file() || directory.join("pnpm-workspace.yaml").is_file() {
            return Ok(directory.to_owned());
        }
        if let Some(package) = read_optional_json(&directory.join("package.json"))? {
            if package.get("workspaces").is_some() {
                return Ok(directory.to_owned());
            }
            if single_package.is_none() && package.get("nx").is_some() {
                single_package = Some(directory.to_owned());
            }
        }
        // A standalone project.json also works, but a parent workspace wins.
        if single_package.is_none() && directory.join("project.json").is_file() {
            single_package = Some(directory.to_owned());
        }
    }
    single_package.with_context(|| {
        format!(
            "no workspace found from {}; use --workspace <path> to select one",
            start.display()
        )
    })
}

#[derive(Default, Deserialize)]
struct PnpmWorkspace {
    #[serde(default)]
    packages: Option<Vec<String>>,
}

/// Which package.json files are projects, as Nx's package.json plugin
/// decides it.
struct Workspaces {
    positive: Vec<(String, Minimatch)>,
    negative: Vec<(String, Minimatch)>,
    /// `NX_INFER_ALL_PACKAGE_JSONS`: every package.json is a project.
    all: bool,
}

impl Workspaces {
    /// Nx's `getGlobPatternsFromPackageManagerWorkspaces` and
    /// `buildPackageJsonPatterns`: the patterns of package.json
    /// `workspaces`, `pnpm-workspace.yaml` and `lerna.json` together, each
    /// naming package.json files, and the root package.json when it has an
    /// `nx` object.
    fn read(root: &Path, package: Option<&Value>, warnings: &mut Vec<String>) -> Result<Self> {
        // Nx only reads the variable when the workspace has no root
        // package.json.
        let all = package.is_none()
            && std::env::var("NX_INFER_ALL_PACKAGE_JSONS").is_ok_and(|value| value == "true");
        let mut patterns = Vec::new();
        if let Some(package) = package {
            let workspaces = package.get("workspaces");
            let entries = match workspaces {
                Some(Value::Array(_)) => workspaces,
                Some(workspaces) => workspaces.get("packages"),
                None => None,
            };
            if let Some(entries) = entries.filter(|entries| !entries.is_null()) {
                let entries: Vec<String> = serde_json::from_value(entries.clone()).context(
                    "package.json workspaces must be an array or an object with a packages array",
                )?;
                patterns.extend(entries);
            }
        }
        // Nx reads these only beside a root package.json; qk reads them
        // without one too.
        let pnpm_path = root.join("pnpm-workspace.yaml");
        if pnpm_path.is_file() {
            let text = std::fs::read_to_string(&pnpm_path)
                .with_context(|| format!("cannot read {}", pnpm_path.display()))?;
            match serde_yaml_ng::from_str::<Option<PnpmWorkspace>>(&text) {
                Ok(workspace) => patterns.extend(
                    workspace
                        .and_then(|workspace| workspace.packages)
                        .unwrap_or_default(),
                ),
                Err(error) => warnings.push(format!("cannot parse pnpm-workspace.yaml: {error}")),
            }
        }
        if root.join("lerna.json").is_file() {
            match read_optional_json(&root.join("lerna.json")) {
                Ok(lerna) => {
                    let packages: Vec<String> = lerna
                        .as_ref()
                        .and_then(|lerna| lerna.get("packages"))
                        .and_then(|packages| serde_json::from_value(packages.clone()).ok())
                        .unwrap_or_default();
                    if packages.is_empty() {
                        patterns.push("packages/*".to_owned());
                    } else {
                        patterns.extend(packages);
                    }
                }
                Err(error) => warnings.push(format!("{error:#}")),
            }
        }
        let mut patterns: Vec<String> = patterns
            .iter()
            .map(|pattern| manifest_pattern(pattern))
            .collect();
        if package.is_some_and(|package| package.get("nx").is_some()) {
            patterns.push("package.json".to_owned());
        }
        let (negative, mut positive): (Vec<String>, Vec<String>) = patterns
            .into_iter()
            .partition(|pattern| pattern.starts_with('!'));
        // Exclusions alone exclude from every package.json.
        if !negative.is_empty() && (positive.is_empty() || positive == ["package.json"]) {
            positive.push("**/package.json".to_owned());
        }
        let compile = |pattern: String| {
            let matcher = Minimatch::new(&pattern, false);
            (pattern, matcher)
        };
        Ok(Self {
            positive: positive.into_iter().map(compile).collect(),
            negative: negative
                .into_iter()
                .map(|pattern| compile(pattern[1..].to_owned()))
                .collect(),
            all,
        })
    }

    /// Whether the workspaces take in the package.json at `path`,
    /// workspace-relative.
    fn includes(&self, path: &str) -> bool {
        self.all
            || (self
                .positive
                .iter()
                .any(|(pattern, matcher)| pattern == path || matcher.is_match(path))
                && !self
                    .negative
                    .iter()
                    .any(|(pattern, matcher)| pattern == path || matcher.is_match(path)))
    }
}

/// Nx's `normalizePatterns`: a pattern names package.json files, `/package.json`
/// added unless it ends so, normalized as Node's `path.join`, without a
/// leading `./`.
fn manifest_pattern(pattern: &str) -> String {
    if pattern.ends_with("/package.json") {
        return pattern.strip_prefix("./").unwrap_or(pattern).to_owned();
    }
    let mut parts: Vec<&str> = Vec::new();
    for part in pattern.split('/').chain(["package.json"]) {
        match part {
            "" | "." => {}
            ".." if parts.last().is_some_and(|last| *last != "..") => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    let joined = parts.join("/");
    if pattern.starts_with('/') {
        format!("/{joined}")
    } else {
        joined
    }
}

/// The repository's `.gitignore` files above the workspace, as Nx's walker
/// reads them: from the workspace's parent up to the repository root.
fn parent_gitignores(root: &Path) -> Vec<PathBuf> {
    let Some(repository) = root.ancestors().find(|path| path.join(".git").exists()) else {
        return Vec::new();
    };
    root.ancestors()
        .skip(1)
        .take_while(|path| path.starts_with(repository))
        .map(|path| path.join(".gitignore"))
        .filter(|path| path.exists())
        .collect()
}

/// Each directory walked: whether it holds a project.json, and whether the
/// workspaces take in its package.json, when it has one.
type Found = std::sync::Mutex<Result<BTreeMap<PathBuf, (bool, Option<bool>)>>>;

/// Each directory holding a project, and whether the package manager's
/// workspaces take in its package.json. As in Nx, a package.json is a
/// project when the workspaces take it in or a project.json is beside it.
pub(crate) fn project_directories(
    root: &Path,
    warnings: &mut Vec<String>,
) -> Result<BTreeMap<PathBuf, bool>> {
    let package = read_optional_json(&root.join("package.json"))?;
    if package.is_none()
        && !root.join("pnpm-workspace.yaml").is_file()
        && !root.join("nx.json").is_file()
        && !root.join("project.json").is_file()
    {
        bail!(
            "{} has no workspace configuration (nx.json, pnpm-workspace.yaml, package.json or project.json)",
            root.display()
        );
    }
    let workspaces = Workspaces::read(root, package.as_ref(), warnings)?;
    let mut walker = WalkBuilder::new(root);
    walker
        // Nx reads .nxignore with gitignore semantics, alongside .gitignore,
        // and not the `.ignore` files ripgrep reads.
        .add_custom_ignore_filename(".nxignore")
        .ignore(false)
        .hidden(false)
        .parents(false)
        // Unlike Nx, a machine's global Git excludes do not hide projects.
        .git_global(false)
        .require_git(false)
        .follow_links(false);
    for gitignore in parent_gitignores(root) {
        walker.add_ignore(gitignore);
    }
    let walker = walker
        .filter_entry(move |entry| {
            if entry.depth() == 0 {
                return true;
            }
            // Nx's walker leaves these out wherever they are; `.qk` is qk's
            // cache, as `.nx/cache` is Nx's.
            let path = entry.path();
            let name = entry.file_name().to_str();
            let parent = path
                .parent()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str());
            !matches!(name, Some("node_modules" | ".git" | ".qk"))
                && !(parent == Some(".nx") && matches!(name, Some("cache" | "workspace-data")))
                && !(parent == Some(".yarn") && name == Some("cache"))
        })
        .threads(4)
        .build_parallel();
    let found: Found = std::sync::Mutex::new(Ok(BTreeMap::new()));
    walker.run(|| {
        Box::new(|entry| {
            // A project.json makes a project; a package.json says whether
            // the workspaces take it in.
            let visit = || -> Result<Option<(PathBuf, Option<bool>)>> {
                let entry = entry.context("cannot walk workspace")?;
                if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                    return Ok(None);
                }
                let directory = entry
                    .path()
                    .parent()
                    .context("configuration has no parent directory")?;
                match entry.file_name().to_str() {
                    Some("project.json") => {
                        return Ok(Some((directory.to_owned(), None)));
                    }
                    Some("package.json") => {
                        let manifest = relative_path(root, entry.path())?;
                        return Ok(Some((
                            directory.to_owned(),
                            Some(workspaces.includes(&manifest)),
                        )));
                    }
                    _ => {}
                }
                Ok(None)
            };
            match visit() {
                Ok(Some((directory, manifest))) => {
                    if let Ok(found) = &mut *found.lock().unwrap() {
                        let entry = found.entry(directory).or_default();
                        match manifest {
                            None => entry.0 = true,
                            Some(included) => entry.1 = Some(included),
                        }
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    *found.lock().unwrap() = Err(error);
                    return ignore::WalkState::Quit;
                }
            }
            ignore::WalkState::Continue
        })
    });
    Ok(found
        .into_inner()
        .unwrap()?
        .into_iter()
        .filter(|(_, (project_json, included))| *project_json || *included == Some(true))
        .map(|(directory, (_, included))| (directory, included == Some(true)))
        .collect())
}
