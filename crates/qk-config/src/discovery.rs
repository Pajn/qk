use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use globset::{GlobBuilder, GlobMatcher};
use ignore::WalkBuilder;
use serde::Deserialize;

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
    packages: Vec<String>,
}

pub(crate) fn project_directories(root: &Path) -> Result<BTreeSet<PathBuf>> {
    let package = read_optional_json(&root.join("package.json"))?;
    let pnpm_path = root.join("pnpm-workspace.yaml");
    let patterns: Vec<String> = if pnpm_path.is_file() {
        let text = std::fs::read_to_string(&pnpm_path)
            .with_context(|| format!("cannot read {}", pnpm_path.display()))?;
        serde_yaml_ng::from_str::<PnpmWorkspace>(&text)
            .with_context(|| format!("cannot parse {}", pnpm_path.display()))?
            .packages
    } else if let Some(workspaces) = package.as_ref().and_then(|p| p.get("workspaces")) {
        let entries = workspaces.get("packages").unwrap_or(workspaces);
        serde_json::from_value(entries.clone()).context(
            "package.json workspaces must be an array or an object with a packages array",
        )?
    } else {
        Vec::new()
    };
    if package.is_none()
        && !pnpm_path.is_file()
        && !root.join("nx.json").is_file()
        && !root.join("project.json").is_file()
    {
        bail!(
            "{} has no workspace configuration (nx.json, pnpm-workspace.yaml, package.json or project.json)",
            root.display()
        );
    }
    let patterns = patterns
        .iter()
        .map(|pattern| {
            let (exclude, pattern) = match pattern.strip_prefix('!') {
                Some(pattern) => (true, pattern),
                None => (false, pattern.as_str()),
            };
            let pattern = pattern
                .strip_prefix("./")
                .unwrap_or(pattern)
                .trim_end_matches('/');
            let matcher = GlobBuilder::new(pattern)
                .literal_separator(true)
                .build()
                .with_context(|| format!("invalid workspace package glob {pattern:?}"))?
                .compile_matcher();
            Ok((exclude, matcher))
        })
        .collect::<Result<Vec<(bool, GlobMatcher)>>>()?;

    let mut directories = BTreeSet::new();
    if package.as_ref().is_some_and(|p| p.get("nx").is_some()) {
        directories.insert(root.to_owned());
    }
    let build_directory = root.join("target");
    let walker = WalkBuilder::new(root)
        // Nx reads .nxignore with gitignore semantics, alongside .gitignore.
        .add_custom_ignore_filename(".nxignore")
        .hidden(false)
        .parents(false)
        .git_global(false)
        .require_git(false)
        .follow_links(false)
        .filter_entry(move |entry| {
            if entry.depth() == 0 {
                return true;
            }
            !matches!(
                entry.file_name().to_str(),
                Some("node_modules" | ".git" | ".qk" | ".nx" | ".pnpm-store")
            ) && entry.path() != build_directory
        })
        .build();
    for entry in walker {
        let entry = entry.context("cannot walk workspace")?;
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let directory = entry
            .path()
            .parent()
            .context("configuration has no parent directory")?;
        match entry.file_name().to_str() {
            Some("project.json") => {
                directories.insert(directory.to_owned());
            }
            Some("package.json") => {
                let relative = relative_path(root, directory)?;
                let included = patterns
                    .iter()
                    .any(|(exclude, glob)| !exclude && glob.is_match(&relative));
                let excluded = patterns
                    .iter()
                    .any(|(exclude, glob)| *exclude && glob.is_match(&relative));
                if included && !excluded {
                    directories.insert(directory.to_owned());
                }
            }
            _ => {}
        }
    }
    Ok(directories)
}
